#![allow(clippy::expect_used)]

use super::memory::{PreparationPolicy, QueryPolicy};
use super::*;
use crate::fts::control::{GuardedVec, TestStage, with_test_stage_hook};
use crate::fts::tokenizer::TokenizerConfig;
use crate::lifecycle::{
    CancelToken, Deadline, OpenOptions, QueryControl, QueryError, SnapshotLease, Store,
};
use crate::property_graph::query::resources::{MemoryError, QueryMemory};
use crate::property_graph::query::runtime::{
    RetainedView, RuntimeContext, RuntimeError, RuntimeLimits,
};
use crate::property_graph::query::{QueryError as GraphQueryError, QueryView};
use crate::property_graph::resources::GraphResources;
use crate::property_graph::staging::{WriteLimits, WriteMemory};
use crate::property_graph::storage::tree::directory::TreeResources;
use crate::property_graph::{GraphGeneration, StoreInstanceId};

struct View {
    token: QueryView,
    lease: SnapshotLease,
}

impl RetainedView for View {
    fn query_view(&self) -> &QueryView {
        &self.token
    }

    fn check_active(&self) -> Result<(), GraphQueryError> {
        self.lease
            .check_active()
            .map_err(|_| GraphQueryError::ReadCancelled)
    }
}

fn view(store: &Store) -> View {
    View {
        token: QueryView::new(
            StoreInstanceId::new(1).expect("store identity"),
            GraphGeneration::new(0),
        ),
        lease: store.snapshot().expect("snapshot"),
    }
}

#[test]
fn lexical_capacity_is_owned_through_finish_decode_and_drop() {
    let directory = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(128 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("resources");
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("writer");
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("memory");
    let mut resources = TreeResources::for_prepare(&memory, 100_000_000).expect("tree");
    let baseline = memory.reserved_bytes();

    {
        let mut policy = PreparationPolicy::new(&memory, &mut resources).expect("growth policy");
        let mut values =
            GuardedVec::<u64, Charge<'_>>::with_capacity(&mut policy, 1).expect("initial vector");
        values.push(1, &mut policy).expect("initial value");
        let old_bytes = values.actual_capacity_bytes();
        assert_eq!(memory.reserved_bytes() - baseline, old_bytes);
        values.push(2, &mut policy).expect("growth");
        let replacement_bytes = values.actual_capacity_bytes();
        assert_eq!(memory.reserved_bytes() - baseline, replacement_bytes);
        assert_eq!(
            memory.peak_reserved_bytes(),
            baseline + old_bytes + replacement_bytes,
            "growth must keep the old and replacement backings charged together",
        );
    }
    assert_eq!(memory.reserved_bytes(), baseline);

    let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("analyzer");
    let mut builder =
        GraphLexicalBuilder::new(&analyzer, &memory, &mut resources).expect("builder");
    builder
        .push_text("bronze zeppelin", &mut resources)
        .expect("row 0");
    builder
        .push_text("silver zeppelin", &mut resources)
        .expect("row 1");

    let nested = builder
        .terms
        .as_slice()
        .iter()
        .fold(0_usize, |total, entry| {
            let positions = entry
                .postings
                .as_slice()
                .iter()
                .fold(0_usize, |sum, posting| {
                    sum.saturating_add(
                        posting
                            .posting
                            .positions
                            .capacity()
                            .saturating_mul(std::mem::size_of::<u32>()),
                    )
                });
            total
                .saturating_add(entry.term.actual_capacity_bytes())
                .saturating_add(entry.postings.actual_capacity_bytes())
                .saturating_add(positions)
        });
    let builder_actual = std::mem::size_of::<GraphLexicalBuilder<'_, '_>>()
        .saturating_add(builder.terms.actual_capacity_bytes())
        .saturating_add(builder.row_lengths.actual_capacity_bytes())
        .saturating_add(nested);
    assert_eq!(memory.reserved_bytes() - baseline, builder_actual);

    let prepared = builder.finish(&mut resources).expect("finish");
    let prepared_actual = prepared
        .region
        .capacity()
        .saturating_add(prepared.decoded.sealed.actual_capacity_bytes())
        .saturating_add(std::mem::size_of::<PreparedGraphLexical<'_>>())
        .saturating_add(std::mem::size_of::<DecodedGraphLexical<'_>>());
    assert_eq!(memory.reserved_bytes() - baseline, prepared_actual);

    let decoded = DecodedGraphLexical::decode_prepare(
        prepared.region(),
        analyzer.epoch(),
        &memory,
        &mut resources,
    )
    .expect("decode");
    let decoded_actual = decoded
        .sealed
        .actual_capacity_bytes()
        .saturating_add(std::mem::size_of::<DecodedGraphLexical<'_>>());
    assert_eq!(
        memory.reserved_bytes() - baseline,
        prepared_actual + decoded_actual
    );
    drop(decoded);
    drop(prepared);
    assert_eq!(memory.reserved_bytes(), baseline);

    let tight_limit = std::mem::size_of::<StorageMemory<'_>>()
        + 256 * 1024
        + std::mem::size_of::<GraphLexicalBuilder<'_, '_>>();
    let tight = StorageMemory::new(&writer, &control, tight_limit).expect("tight memory");
    let mut tight_resources = TreeResources::for_prepare(&tight, 100_000_000).expect("tight tree");
    let tight_baseline = tight.reserved_bytes();
    let mut tight_builder = GraphLexicalBuilder::new(&analyzer, &tight, &mut tight_resources)
        .expect("descriptor exactly fits");
    assert!(matches!(
        tight_builder.push_text("bronze", &mut tight_resources),
        Err(GraphLexicalError::Resource(TreeError::Memory))
    ));
    drop(tight_builder);
    assert_eq!(tight.reserved_bytes(), tight_baseline);

    let retained = view(&store);
    let initial_elements = 8_usize;
    let initial_bytes = initial_elements.saturating_mul(std::mem::size_of::<u64>());
    let query_limit = std::mem::size_of::<QueryMemory<'_>>()
        .saturating_add(std::mem::size_of::<RuntimeContext<'_, '_, '_>>())
        .saturating_add(initial_bytes);
    let query_memory = QueryMemory::new(&shared, query_limit).expect("tight query memory");
    let query_baseline = query_memory.reserved_bytes();
    let query_control = QueryControl::Cancel(CancelToken::new());
    let mut context = RuntimeContext::new(
        &retained,
        &query_control,
        &query_memory,
        RuntimeLimits::default(),
    )
    .expect("query runtime");
    let context_bytes = query_memory.reserved_bytes();
    {
        let mut policy = QueryPolicy::new(&query_memory, &mut context).expect("query policy");
        let mut values =
            GuardedVec::<u64, Charge<'_>>::with_capacity(&mut policy, initial_elements)
                .expect("query vector");
        assert_eq!(values.actual_capacity_bytes(), initial_bytes);
        for value in 0..initial_elements {
            values
                .push(value as u64, &mut policy)
                .expect("fill query vector");
        }
        let before_growth = query_memory.reserved_bytes();
        assert!(matches!(
            values.push(99, &mut policy),
            Err(GraphLexicalError::Resource(TreeError::Runtime(
                RuntimeError::Memory(MemoryError::Limit)
            )))
        ));
        assert_eq!(values.len(), initial_elements);
        assert_eq!(values.actual_capacity_bytes(), initial_bytes);
        assert_eq!(query_memory.reserved_bytes(), before_growth);
    }
    assert_eq!(query_memory.reserved_bytes(), context_bytes);
    drop(context);
    assert_eq!(query_memory.reserved_bytes(), query_baseline);
    drop(retained);
    drop(query_memory);

    drop(tight_resources);
    drop(tight);
    drop(resources);
    drop(memory);
    drop(shared);
    store.close().expect("close");
}

fn assert_cancelled<T>(result: Result<T, GraphLexicalError>) {
    assert!(matches!(
        result,
        Err(GraphLexicalError::Resource(TreeError::Control(
            QueryError::Cancelled { partial: false }
        )))
    ));
}

#[test]
fn lexical_long_work_stops_inside_analysis_sort_and_codec() {
    let directory = tempfile::tempdir().expect("store fixture");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("resources");
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("writer");
    let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("analyzer");
    let long_token = "a".repeat(16 * 1024);
    let repeated_terms = (0..64)
        .flat_map(|round| (0..32).map(move |term| format!("term{term}r{}", round % 2)))
        .collect::<Vec<_>>()
        .join(" ");
    let positions = "zeppelin ".repeat(1_024);
    let region_text = "bronze zeppelin";

    {
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("memory");
        let mut resources = TreeResources::for_prepare(&memory, 100_000_000).expect("tree");
        let baseline = memory.reserved_bytes();
        let mut builder =
            GraphLexicalBuilder::new(&analyzer, &memory, &mut resources).expect("analysis builder");
        let probes = std::rc::Rc::new(std::cell::Cell::new(0_usize));
        let observed = std::rc::Rc::clone(&probes);
        let cancel = token.clone();
        let result = with_test_stage_hook(
            move |stage| {
                if stage == TestStage::Analysis {
                    observed.set(observed.get().saturating_add(1));
                    cancel.cancel();
                }
            },
            || builder.push_text(&long_token, &mut resources),
        );
        assert!(probes.get() > 0);
        assert_cancelled(result);
        drop(builder);
        assert_eq!(memory.reserved_bytes(), baseline);
    }

    {
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("memory");
        let mut resources = TreeResources::for_prepare(&memory, 100_000_000).expect("tree");
        let baseline = memory.reserved_bytes();
        let mut builder =
            GraphLexicalBuilder::new(&analyzer, &memory, &mut resources).expect("sort builder");
        builder
            .push_text(&repeated_terms, &mut resources)
            .expect("sort input");
        let probes = std::rc::Rc::new(std::cell::Cell::new(0_usize));
        let observed = std::rc::Rc::clone(&probes);
        let cancel = token.clone();
        let result = with_test_stage_hook(
            move |stage| {
                if stage == TestStage::Sort {
                    observed.set(observed.get().saturating_add(1));
                    cancel.cancel();
                }
            },
            || builder.finish(&mut resources),
        );
        assert!(probes.get() > 0);
        assert_cancelled(result);
        assert_eq!(memory.reserved_bytes(), baseline);
    }

    for stage in [TestStage::PositionsEncode, TestStage::RegionEncode] {
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("memory");
        let mut resources = TreeResources::for_prepare(&memory, 100_000_000).expect("tree");
        let baseline = memory.reserved_bytes();
        let mut builder =
            GraphLexicalBuilder::new(&analyzer, &memory, &mut resources).expect("codec builder");
        let input = if stage == TestStage::PositionsEncode {
            positions.as_str()
        } else {
            region_text
        };
        builder
            .push_text(input, &mut resources)
            .expect("codec input");
        let probes = std::rc::Rc::new(std::cell::Cell::new(0_usize));
        let observed = std::rc::Rc::clone(&probes);
        let cancel = token.clone();
        let result = with_test_stage_hook(
            move |actual| {
                if actual == stage {
                    observed.set(observed.get().saturating_add(1));
                    cancel.cancel();
                }
            },
            || builder.finish(&mut resources),
        );
        assert!(probes.get() > 0, "{stage:?}");
        assert_cancelled(result);
        assert_eq!(memory.reserved_bytes(), baseline, "{stage:?}");
    }

    let (position_region, ordinary_region) = {
        let control = QueryControl::Cancel(CancelToken::new());
        let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("memory");
        let mut resources = TreeResources::for_prepare(&memory, 100_000_000).expect("tree");
        let mut position_builder =
            GraphLexicalBuilder::new(&analyzer, &memory, &mut resources).expect("position builder");
        position_builder
            .push_text(&positions, &mut resources)
            .expect("positions");
        let position_prepared = position_builder
            .finish(&mut resources)
            .expect("position region");
        let mut ordinary_builder =
            GraphLexicalBuilder::new(&analyzer, &memory, &mut resources).expect("region builder");
        ordinary_builder
            .push_text(region_text, &mut resources)
            .expect("region text");
        let ordinary_prepared = ordinary_builder
            .finish(&mut resources)
            .expect("ordinary region");
        (
            position_prepared.region().to_vec(),
            ordinary_prepared.region().to_vec(),
        )
    };

    for stage in [TestStage::PositionsDecode, TestStage::RegionDecode] {
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("memory");
        let mut resources = TreeResources::for_prepare(&memory, 100_000_000).expect("tree");
        let baseline = memory.reserved_bytes();
        let bytes = if stage == TestStage::PositionsDecode {
            position_region.as_slice()
        } else {
            ordinary_region.as_slice()
        };
        let probes = std::rc::Rc::new(std::cell::Cell::new(0_usize));
        let observed = std::rc::Rc::clone(&probes);
        let cancel = token.clone();
        let result = with_test_stage_hook(
            move |actual| {
                if actual == stage {
                    observed.set(observed.get().saturating_add(1));
                    cancel.cancel();
                }
            },
            || {
                DecodedGraphLexical::decode_prepare(
                    bytes,
                    analyzer.epoch(),
                    &memory,
                    &mut resources,
                )
            },
        );
        assert!(probes.get() > 0, "{stage:?}");
        assert_cancelled(result);
        assert_eq!(memory.reserved_bytes(), baseline, "{stage:?}");
    }

    {
        let control = QueryControl::Cancel(CancelToken::new());
        let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("memory");
        let mut resources = TreeResources::for_prepare(&memory, 64).expect("tight work");
        let baseline = memory.reserved_bytes();
        let mut builder =
            GraphLexicalBuilder::new(&analyzer, &memory, &mut resources).expect("tight builder");
        let probes = std::rc::Rc::new(std::cell::Cell::new(0_usize));
        let observed = std::rc::Rc::clone(&probes);
        let result = with_test_stage_hook(
            move |stage| {
                if stage == TestStage::Work {
                    observed.set(observed.get().saturating_add(1));
                }
            },
            || builder.push_text(&long_token, &mut resources),
        );
        assert!(probes.get() > 0);
        assert!(matches!(
            result,
            Err(GraphLexicalError::Resource(TreeError::Work))
        ));
        drop(builder);
        assert_eq!(memory.reserved_bytes(), baseline);
    }

    {
        let control = QueryControl::Cancel(CancelToken::new());
        let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("memory");
        let mut resources = TreeResources::for_prepare(&memory, 100_000_000).expect("tree");
        let mut builder = GraphLexicalBuilder::new(&analyzer, &memory, &mut resources)
            .expect("successful builder");
        for input in [&long_token, &repeated_terms, &positions] {
            assert!(
                builder
                    .push_text(input, &mut resources)
                    .expect("successful row")
                    .is_some()
            );
        }
        assert!(
            builder
                .push_text(region_text, &mut resources)
                .expect("successful row")
                .is_some()
        );
        let prepared = builder.finish(&mut resources).expect("successful encode");
        let decoded = DecodedGraphLexical::decode_prepare(
            prepared.region(),
            analyzer.epoch(),
            &memory,
            &mut resources,
        )
        .expect("successful decode");
        assert_eq!(decoded.row_count(), 4);
    }

    let expired =
        QueryControl::Deadline(Deadline::after(std::time::Duration::ZERO).expect("deadline"));
    assert!(matches!(
        StorageMemory::new(&writer, &expired, 32 * 1024 * 1024),
        Err(TreeError::Control(QueryError::Timeout { partial: false }))
    ));

    drop(shared);
    store.close().expect("close");
}
