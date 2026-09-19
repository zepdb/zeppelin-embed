#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::query::completed::*;
use zeppelin_embed::property_graph::query::resources::QueryMemory;
use zeppelin_embed::property_graph::query::runtime::{
    RetainedView, RuntimeContext, RuntimeLimits, WorkKind,
};
use zeppelin_embed::property_graph::query::{QueryError, QueryView};
use zeppelin_embed::property_graph::resources::GraphResources;
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};

#[cfg(not(feature = "allocation-audit"))]
#[path = "graph_completed_results/allocations.rs"]
mod allocations;

struct View(QueryView);
impl RetainedView for View {
    fn query_view(&self) -> &QueryView {
        &self.0
    }
    fn check_active(&self) -> Result<(), QueryError> {
        Ok(())
    }
}
struct Source<'a>(ResultInput<'a>);
impl ResultSource for Source<'_> {
    fn result_input(&self) -> Result<ResultInput<'_>, SourceError> {
        Ok(self.0)
    }
}

#[test]
fn completed_scalar_table_owns_exact_bits_and_bytes_after_sources_are_gone() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(8 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let baseline = shared.reserved_bytes().unwrap();
    let result = {
        let memory = QueryMemory::new(&shared, 1024 * 1024).unwrap();
        let view = View(QueryView::new(
            StoreInstanceId::new(9).unwrap(),
            GraphGeneration::new(7),
        ));
        let control = QueryControl::Cancel(CancelToken::new());
        let mut context =
            RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()).unwrap();
        let base = memory.reserved_bytes();
        let bytes = String::from("v\0λ");
        let values = [
            Value::Null,
            Value::F64(0x7ff8_0000_0000_1234),
            Value::String(Span::new(1, 3)),
        ];
        let columns = [Column {
            name: Span::new(0, 1),
            kinds: ValueKinds::ANY,
        }];
        let cells = [ValueIndex(0), ValueIndex(1), ValueIndex(2)];
        let source = Source(ResultInput {
            view: &view.0,
            rows: 3,
            outcome: Outcome::Read,
            pools: Pools {
                values: &values,
                bytes: bytes.as_bytes(),
                columns: &columns,
                cells: &cells,
                ..Pools::default()
            },
        });
        let prepared = PreparedGraphResult::copy_from(&source, &mut context).unwrap();
        assert!(memory.reserved_bytes() > base);
        let pointer = prepared.pools().bytes.as_ptr();
        let result = prepared.detach(context.counters(), memory.peak_reserved_bytes());
        assert_eq!(pointer, result.pools().bytes.as_ptr());
        assert_eq!(memory.reserved_bytes(), base);
        assert_eq!(
            result.metadata().counters.get(WorkKind::CompletedRows),
            0,
            "driver owns completion counters"
        );
        result
    };
    assert_eq!(shared.reserved_bytes().unwrap(), baseline);
    assert_eq!(result.cell(0, 0), Some(&Value::Null));
    assert_eq!(result.cell(1, 0), Some(&Value::F64(0x7ff8_0000_0000_1234)));
    assert_eq!(result.string(Span::new(1, 3)), Some("\0λ"));
    assert_eq!(result.metadata().generation, GraphGeneration::new(7));
    store.close().unwrap();
    assert_eq!(result.cell(2, 0), Some(&Value::String(Span::new(1, 3))));
}

#[test]
fn completed_graph_lists_entities_and_reports_preserve_full_literal_contents() {
    use zeppelin_embed::property_graph::query::plan::SearchCallId;
    use zeppelin_embed::property_graph::{EntityKind, GraphRevision, NodeId, RelId};
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(8 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let memory = QueryMemory::new(&shared, 1024 * 1024).unwrap();
    let view = View(QueryView::new(
        StoreInstanceId::new(9).unwrap(),
        GraphGeneration::new(7),
    ));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context =
        RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()).unwrap();
    let node_id = NodeId::new((1_u128 << 100) + 7).unwrap();
    let other_id = NodeId::new((2_u128 << 100) + 7).unwrap();
    let key = Key {
        kind: EntityKind::Node,
        namespace: Span::new(0, 0),
        value: Span::new(1, 1),
    };
    let nodes = [Node {
        id: node_id,
        revision: GraphRevision::new(3).unwrap(),
        generation: GraphGeneration::new(6),
        key: Some(key),
        labels: Span::new(0, 1),
        properties: Span::new(0, 1),
        text: None,
        vector: None,
    }];
    let relationships = [Relationship {
        id: RelId::new((3_u128 << 100) + 7).unwrap(),
        revision: GraphRevision::new(2).unwrap(),
        generation: GraphGeneration::new(5),
        key: None,
        source: node_id,
        target: other_id,
        relationship_type: Span::new(2, 6),
        properties: Span::new(0, 1),
    }];
    let values = [
        Value::Node(0),
        Value::Relationship(0),
        Value::List {
            children: Span::new(0, 0),
            element: ListKind::Empty,
        },
        Value::Bool(true),
        Value::F64(0x8000_0000_0000_0000),
        Value::List {
            children: Span::new(0, 3),
            element: ListKind::Query,
        },
    ];
    let columns = [Column {
        name: Span::new(0, 1),
        kinds: ValueKinds::ANY,
    }; 3];
    let names = [Span::new(2, 6)];
    let properties = [Property {
        name: Span::new(0, 1),
        value: ValueIndex(4),
    }];
    let children = [ValueIndex(2), ValueIndex(3), ValueIndex(4)];
    let cells = [ValueIndex(0), ValueIndex(1), ValueIndex(5)];
    let reports = [SearchReport {
        call: SearchCallId(0),
        generation: GraphGeneration::new(7),
        kind: SearchKind::Hybrid,
        requested_tier: None,
        actual_tier: Some(ActualTier::Graph),
        precision: ScorePrecision::Original,
        coverage: CandidateCoverage::Approximate,
        vector_leg: LegState::Nonempty,
        lexical_leg: LegState::NoQueryMatches,
        document_epoch: Some(19),
        query_epoch: Some(20),
        tokenizer_epoch: Some(21),
        effective_alpha_bits: 1_f64.to_bits(),
        normalization_version: 1,
        rules_version: 1,
        candidate_count: 3,
        cross_scored_count: 3,
        fallback_count: 2,
        cross_score_complete: true,
        work: context.counters(),
    }];
    let source = Source(ResultInput {
        view: &view.0,
        rows: 1,
        outcome: Outcome::Read,
        pools: Pools {
            values: &values,
            bytes: b"p\0Person",
            columns: &columns,
            cells: &cells,
            children: &children,
            names: &names,
            properties: &properties,
            nodes: &nodes,
            relationships: &relationships,
            reports: &reports,
            ..Pools::default()
        },
    });
    let prepared = PreparedGraphResult::copy_from(&source, &mut context).unwrap();
    let result = prepared.detach(context.counters(), memory.peak_reserved_bytes());
    assert_eq!(result.pools().nodes, nodes);
    assert_eq!(result.pools().relationships, relationships);
    assert_eq!(result.pools().values, values);
    assert_eq!(result.pools().children, children);
    assert_eq!(result.pools().reports, reports);
    assert_eq!(
        result.string(result.pools().nodes[0].key.unwrap().value),
        Some("\0")
    );
    assert_eq!(
        result.pools().nodes[0].id.get(),
        1267650600228229401496703205383
    );
    assert_eq!(
        result.pools().relationships[0].target.get(),
        2535301200456458802993406410759
    );
    assert_eq!(result.pools().nodes[0].text, None);
    assert_eq!(result.pools().nodes[0].vector, None);
}

fn context_case(run: impl FnOnce(&mut RuntimeContext<'_, '_, '_>)) {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(64 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let memory = QueryMemory::new(&shared, 24 * 1024 * 1024).unwrap();
    let view = View(QueryView::new(
        StoreInstanceId::new(9).unwrap(),
        GraphGeneration::new(7),
    ));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context =
        RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()).unwrap();
    let base = memory.reserved_bytes();
    run(&mut context);
    assert_eq!(
        memory.reserved_bytes(),
        base,
        "every owned allocation/guard released"
    );
}

#[test]
fn malformed_lists_and_ranges_never_expose_partial_owned_results() {
    context_case(|context| {
        let children = [ValueIndex(0)];
        let column = [Column {
            name: Span::new(0, 0),
            kinds: ValueKinds::ANY,
        }];
        let cells = [ValueIndex(0)];
        for invalid in [
            Value::List {
                children: Span::new(0, 1),
                element: ListKind::Query,
            }, // self-cycle
            Value::List {
                children: Span::new(0, 1),
                element: ListKind::Empty,
            },
            Value::List {
                children: Span::new(u32::MAX, 1),
                element: ListKind::Query,
            },
            Value::Node(0),
            Value::Relationship(0),
            Value::String(Span::new(0, 1)),
        ] {
            let values = [invalid];
            let source = Source(ResultInput {
                view: context.view(),
                rows: 1,
                outcome: Outcome::Read,
                pools: Pools {
                    values: &values,
                    children: &children,
                    columns: &column,
                    cells: &cells,
                    ..Pools::default()
                },
            });
            assert!(
                PreparedGraphResult::copy_from(&source, context).is_err(),
                "malformed descriptor must fail: {invalid:?}"
            );
        }
        let values = [
            Value::Bool(true),
            Value::List {
                children: Span::new(0, 1),
                element: ListKind::I64,
            },
        ];
        let source = Source(ResultInput {
            view: context.view(),
            rows: 1,
            outcome: Outcome::Read,
            pools: Pools {
                values: &values,
                children: &children,
                columns: &column,
                cells: &[ValueIndex(1)],
                ..Pools::default()
            },
        });
        assert!(
            PreparedGraphResult::copy_from(&source, context).is_err(),
            "stored list cannot silently widen its element kind"
        );
        let mut deep = vec![Value::Null];
        let mut links = Vec::new();
        for i in 0..17 {
            links.push(ValueIndex(i));
            deep.push(Value::List {
                children: Span::new(i, 1),
                element: ListKind::Query,
            });
        }
        let source = Source(ResultInput {
            view: context.view(),
            rows: 1,
            outcome: Outcome::Read,
            pools: Pools {
                values: &deep,
                children: &links,
                columns: &column,
                cells: &[ValueIndex(17)],
                ..Pools::default()
            },
        });
        assert!(
            PreparedGraphResult::copy_from(&source, context).is_err(),
            "seventeenth nested list rejects"
        );
    });
}

#[test]
fn malformed_entity_metadata_and_unreferenced_pool_entries_reject() {
    use zeppelin_embed::property_graph::{EntityKind, GraphRevision, NodeId};
    context_case(|context| {
        let good = Node {
            id: NodeId::new(17).unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            generation: GraphGeneration::new(7),
            key: None,
            labels: Span::new(0, 0),
            properties: Span::new(0, 0),
            text: None,
            vector: None,
        };
        for bad in [
            Node {
                key: Some(Key {
                    kind: EntityKind::Relationship,
                    namespace: Span::new(0, 0),
                    value: Span::new(0, 0),
                }),
                ..good
            },
            Node {
                generation: GraphGeneration::new(8),
                ..good
            },
            Node {
                labels: Span::new(0, 2),
                ..good
            },
            Node {
                properties: Span::new(0, 1),
                ..good
            },
            Node {
                text: Some(Span::new(0, 1)),
                ..good
            },
            Node {
                vector: Some(Span::new(0, 0)),
                ..good
            },
        ] {
            let nodes = [bad];
            let source = Source(ResultInput {
                view: context.view(),
                rows: 0,
                outcome: Outcome::Read,
                pools: Pools {
                    nodes: &nodes,
                    ..Pools::default()
                },
            });
            assert!(
                PreparedGraphResult::copy_from(&source, context).is_err(),
                "complete entity metadata must validate: {bad:?}"
            );
        }
        let source = Source(ResultInput {
            view: context.view(),
            rows: 0,
            outcome: Outcome::Read,
            pools: Pools {
                names: &[Span::new(0, 1)],
                ..Pools::default()
            },
        });
        assert!(
            PreparedGraphResult::copy_from(&source, context).is_err(),
            "unreferenced names are still part of the represented result"
        );
        let source = Source(ResultInput {
            view: context.view(),
            rows: 0,
            outcome: Outcome::Read,
            pools: Pools {
                children: &[ValueIndex(999)],
                ..Pools::default()
            },
        });
        assert!(
            PreparedGraphResult::copy_from(&source, context).is_err(),
            "unreferenced child indices must not escape validation"
        );
    });
}

fn lexical_report() -> SearchReport {
    SearchReport {
        call: zeppelin_embed::property_graph::query::plan::SearchCallId(0),
        generation: GraphGeneration::new(7),
        kind: SearchKind::Lexical,
        requested_tier: None,
        actual_tier: None,
        precision: ScorePrecision::NotApplicable,
        coverage: CandidateCoverage::Exact,
        vector_leg: LegState::NotRequested,
        lexical_leg: LegState::NoQueryMatches,
        document_epoch: None,
        query_epoch: None,
        tokenizer_epoch: Some(17),
        effective_alpha_bits: 0.0_f64.to_bits(),
        normalization_version: 1,
        rules_version: 1,
        candidate_count: 0,
        cross_scored_count: 0,
        fallback_count: 0,
        cross_score_complete: true,
        work: Default::default(),
    }
}

#[test]
fn report_geometry_rejects_wrong_generation_call_and_nonfinite_policy() {
    context_case(|context| {
        for report in [
            SearchReport {
                generation: GraphGeneration::new(8),
                ..lexical_report()
            },
            SearchReport {
                call: zeppelin_embed::property_graph::query::plan::SearchCallId(1),
                ..lexical_report()
            },
            SearchReport {
                effective_alpha_bits: f64::NAN.to_bits(),
                ..lexical_report()
            },
            SearchReport {
                effective_alpha_bits: 1.5_f64.to_bits(),
                ..lexical_report()
            },
            SearchReport {
                cross_scored_count: 1,
                ..lexical_report()
            },
            SearchReport {
                actual_tier: Some(ActualTier::Graph),
                ..lexical_report()
            },
        ] {
            let reports = [report];
            let source = Source(ResultInput {
                view: context.view(),
                rows: 0,
                outcome: Outcome::Read,
                pools: Pools {
                    reports: &reports,
                    ..Pools::default()
                },
            });
            assert!(
                PreparedGraphResult::copy_from(&source, context).is_err(),
                "invalid complete report metadata rejects"
            );
        }
        let reports = [lexical_report()];
        let source = Source(ResultInput {
            view: context.view(),
            rows: 0,
            outcome: Outcome::Read,
            pools: Pools {
                reports: &reports,
                ..Pools::default()
            },
        });
        let prepared = PreparedGraphResult::copy_from(&source, context).unwrap();
        assert_eq!(
            prepared.pools().reports,
            reports,
            "zero result rows preserve eager report"
        );
    });
}

#[test]
fn producer_errors_and_identical_metadata_foreign_tokens_remain_distinct() {
    struct Failed(SourceError);
    impl ResultSource for Failed {
        fn result_input(&self) -> Result<ResultInput<'_>, SourceError> {
            Err(self.0)
        }
    }
    context_case(|context| {
        let id = zeppelin_embed::property_graph::EntityId::Node(
            zeppelin_embed::property_graph::NodeId::new((1_u128 << 96) + 3).unwrap(),
        );
        for error in [
            SourceError::Deleted(id),
            SourceError::Missing(id),
            SourceError::Storage,
            SourceError::ForeignView,
        ] {
            assert!(
                matches!(PreparedGraphResult::copy_from(&Failed(error), context), Err(CompletedError::Source(actual)) if actual == error)
            );
        }
        let foreign = QueryView::new(context.view().store(), context.view().generation());
        let source = Source(ResultInput {
            view: &foreign,
            rows: 0,
            outcome: Outcome::Read,
            pools: Pools::default(),
        });
        assert!(matches!(
            PreparedGraphResult::copy_from(&source, context),
            Err(CompletedError::Source(SourceError::ForeignView))
        ));
    });
}

#[test]
fn expanded_descendants_and_complete_representation_caps_cannot_be_bypassed() {
    context_case(|context| {
        let mut values = vec![Value::Null];
        let mut children = Vec::new();
        for index in 0..12 {
            children.extend([ValueIndex(index); 3]);
            values.push(Value::List {
                children: Span::new(index * 3, 3),
                element: ListKind::Query,
            });
        }
        let source = Source(ResultInput {
            view: context.view(),
            rows: 0,
            outcome: Outcome::Read,
            pools: Pools {
                values: &values,
                children: &children,
                ..Pools::default()
            },
        });
        assert!(
            matches!(
                PreparedGraphResult::copy_from(&source, context),
                Err(CompletedError::Limit)
            ),
            "repeated DAG children count as separate logical descendants"
        );
        let bytes = vec![b'x'; 4 * 1024 * 1024];
        let source = Source(ResultInput {
            view: context.view(),
            rows: 0,
            outcome: Outcome::Read,
            pools: Pools {
                bytes: &bytes,
                ..Pools::default()
            },
        });
        assert!(
            matches!(
                PreparedGraphResult::copy_from(&source, context),
                Err(CompletedError::Limit)
            ),
            "root descriptors count in the complete 4MiB representation"
        );
        let source = Source(ResultInput {
            view: context.view(),
            rows: 65537,
            outcome: Outcome::Read,
            pools: Pools::default(),
        });
        assert!(PreparedGraphResult::copy_from(&source, context).is_err());
    });
}

#[test]
fn copy_limit_records_only_performed_chunks_and_returns_no_partial_owner() {
    use zeppelin_embed::property_graph::query::runtime::RuntimeError;
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(8 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let memory = QueryMemory::new(&shared, 1024 * 1024).unwrap();
    let view = View(QueryView::new(
        StoreInstanceId::new(9).unwrap(),
        GraphGeneration::new(7),
    ));
    let control = QueryControl::Cancel(CancelToken::new());
    let permitted = std::mem::size_of::<Value>() as u64 + 65536;
    let limits = RuntimeLimits::default()
        .with_limit(WorkKind::CopiedBytes, permitted)
        .unwrap();
    let mut context = RuntimeContext::new(&view, &control, &memory, limits).unwrap();
    let baseline = memory.reserved_bytes();
    let bytes = vec![b'x'; 65537];
    let source = Source(ResultInput {
        view: &view.0,
        rows: 0,
        outcome: Outcome::Read,
        pools: Pools {
            bytes: &bytes,
            values: &[Value::String(Span::new(0, 65537))],
            ..Pools::default()
        },
    });
    assert!(matches!(
        PreparedGraphResult::copy_from(&source, &mut context),
        Err(CompletedError::Runtime(RuntimeError::Limit(
            WorkKind::CopiedBytes
        )))
    ));
    assert_eq!(context.counters().get(WorkKind::CopiedBytes), permitted);
    assert_eq!(context.counters().get(WorkKind::CompletedBytes), 0);
    assert_eq!(memory.reserved_bytes(), baseline);
}

#[test]
fn every_actual_validation_and_copy_checkpoint_can_cancel_without_an_owner() {
    use std::cell::Cell;
    struct Controlled {
        view: QueryView,
        checks: Cell<usize>,
        stop: Cell<usize>,
    }
    impl RetainedView for Controlled {
        fn query_view(&self) -> &QueryView {
            &self.view
        }
        fn check_active(&self) -> Result<(), QueryError> {
            let n = self.checks.get() + 1;
            self.checks.set(n);
            if n >= self.stop.get() {
                Err(QueryError::ReadCancelled)
            } else {
                Ok(())
            }
        }
    }
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(8 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let memory = QueryMemory::new(&shared, 1024 * 1024).unwrap();
    let view = Controlled {
        view: QueryView::new(StoreInstanceId::new(9).unwrap(), GraphGeneration::new(7)),
        checks: Cell::new(0),
        stop: Cell::new(usize::MAX),
    };
    let token = CancelToken::new();
    let control = QueryControl::Cancel(token.clone());
    let mut bytes = vec![b'a'; 131072];
    bytes[65535] = 0xce;
    bytes[65536] = 0xbb; // UTF-8 lambda spans a control boundary.
    let values = [Value::String(Span::new(0, bytes.len() as u32))];
    let source = Source(ResultInput {
        view: &view.view,
        rows: 0,
        outcome: Outcome::Read,
        pools: Pools {
            values: &values,
            bytes: &bytes,
            ..Pools::default()
        },
    });
    let baseline = memory.reserved_bytes();
    let total = {
        let mut context =
            RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()).unwrap();
        view.checks.set(0);
        let owner = PreparedGraphResult::copy_from(&source, &mut context).unwrap();
        let count = view.checks.get();
        let result = owner.detach(context.counters(), memory.peak_reserved_bytes());
        assert_eq!(
            &result
                .string(Span::new(0, bytes.len() as u32))
                .unwrap()
                .as_bytes()[65535..65537],
            [0xce, 0xbb]
        );
        count
    };
    assert!(total >= 8);
    assert_eq!(memory.reserved_bytes(), baseline);
    for stop in 1..=total {
        view.stop.set(usize::MAX);
        let mut context =
            RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()).unwrap();
        view.checks.set(0);
        view.stop.set(stop);
        assert!(matches!(
            PreparedGraphResult::copy_from(&source, &mut context),
            Err(CompletedError::Runtime(
                zeppelin_embed::property_graph::query::runtime::RuntimeError::Value(
                    QueryError::ReadCancelled
                )
            ))
        ));
        assert_eq!(view.checks.get(), stop);
        drop(context);
        assert_eq!(memory.reserved_bytes(), baseline);
    }
    view.stop.set(usize::MAX);
    let mut context =
        RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()).unwrap();
    view.checks.set(0);
    view.stop.set(1);
    token.cancel();
    assert!(
        matches!(
            PreparedGraphResult::copy_from(&source, &mut context),
            Err(CompletedError::Runtime(
                zeppelin_embed::property_graph::query::runtime::RuntimeError::Value(
                    QueryError::ReadCancelled
                )
            ))
        ),
        "real retained-view check precedes caller cancellation"
    );
}

#[test]
fn completed_receipts_preserve_original_items_deletion_and_mixed_generations() {
    use zeppelin_embed::property_graph::staging::ItemReceipt;
    use zeppelin_embed::property_graph::{EntityId, GraphRevision, NodeId, RelId};
    context_case(|context| {
        let receipts = [
            Receipt {
                item_index: 0,
                deleted: false,
                receipt: ItemReceipt {
                    entity: EntityId::Node(NodeId::new((1_u128 << 100) + 7).unwrap()),
                    revision: GraphRevision::new(3).unwrap(),
                    generation: GraphGeneration::new(4),
                    replayed: true,
                },
            },
            Receipt {
                item_index: 1,
                deleted: true,
                receipt: ItemReceipt {
                    entity: EntityId::Relationship(RelId::new((1_u128 << 99) + 7).unwrap()),
                    revision: GraphRevision::new(5).unwrap(),
                    generation: GraphGeneration::new(8),
                    replayed: false,
                },
            },
        ];
        let source = Source(ResultInput {
            view: context.view(),
            rows: 0,
            outcome: Outcome::Committed {
                changed: GraphGeneration::new(8),
            },
            pools: Pools {
                receipts: &receipts,
                ..Pools::default()
            },
        });
        let owner = PreparedGraphResult::copy_from(&source, context).unwrap();
        assert_eq!(owner.metadata().generation, GraphGeneration::new(7));
        assert_eq!(
            owner.metadata().outcome,
            Outcome::Committed {
                changed: GraphGeneration::new(8)
            }
        );
        let owned = owner.detach(context.counters(), context.memory().peak_reserved_bytes());
        assert_eq!(owned.pools().receipts, receipts);
        assert!(owned.pools().receipts[1].deleted);
        assert_eq!(owned.pools().receipts[0].receipt.generation.get(), 4);
        let malformed = [Receipt {
            item_index: 1,
            ..receipts[0]
        }];
        let source = Source(ResultInput {
            view: context.view(),
            rows: 0,
            outcome: Outcome::Replayed,
            pools: Pools {
                receipts: &malformed,
                ..Pools::default()
            },
        });
        assert!(
            PreparedGraphResult::copy_from(&source, context).is_err(),
            "complete receipts preserve original item position"
        );
    });
}

#[test]
fn complete_reports_reject_contradictory_kind_route_and_modality_states() {
    context_case(|context| {
        for report in [
            SearchReport {
                lexical_leg: LegState::NotRequested,
                ..lexical_report()
            },
            SearchReport {
                kind: SearchKind::Vector,
                vector_leg: LegState::Nonempty,
                lexical_leg: LegState::NotRequested,
                ..lexical_report()
            },
            SearchReport {
                kind: SearchKind::Vector,
                vector_leg: LegState::Nonempty,
                lexical_leg: LegState::NotRequested,
                actual_tier: Some(ActualTier::Exact),
                ..lexical_report()
            },
            SearchReport {
                kind: SearchKind::Hybrid,
                vector_leg: LegState::Nonempty,
                precision: ScorePrecision::Original,
                ..lexical_report()
            },
            SearchReport {
                kind: SearchKind::Hybrid,
                vector_leg: LegState::NoQueryMatches,
                actual_tier: Some(ActualTier::Exact),
                precision: ScorePrecision::Original,
                ..lexical_report()
            },
        ] {
            let reports = [report];
            let source = Source(ResultInput {
                view: context.view(),
                rows: 0,
                outcome: Outcome::Read,
                pools: Pools {
                    reports: &reports,
                    ..Pools::default()
                },
            });
            assert!(
                PreparedGraphResult::copy_from(&source, context).is_err(),
                "complete report cannot contain contradictory modality/route state: {report:?}"
            );
        }
    });
}

#[test]
fn real_retained_capacity_overlap_enforces_query_and_shared_caps() {
    use zeppelin_embed::property_graph::query::resources::{MemoryError, QueryArena};
    use zeppelin_embed::property_graph::query::runtime::RuntimeError;
    for shared_limit in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let budget = 8 * 1024 * 1024;
        let store = Store::open(
            root.path(),
            OpenOptions::new().with_max_resident_bytes(budget),
        )
        .unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        let store_base = shared.reserved_bytes().unwrap();
        {
            let memory = QueryMemory::new(
                &shared,
                if shared_limit {
                    24 * 1024 * 1024
                } else {
                    262144
                },
            )
            .unwrap();
            let view = View(QueryView::new(
                StoreInstanceId::new(9).unwrap(),
                GraphGeneration::new(7),
            ));
            let control = QueryControl::Cancel(CancelToken::new());
            let mut context =
                RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()).unwrap();
            let capacity = if shared_limit {
                budget as usize - shared.reserved_bytes().unwrap() as usize - 65536
            } else {
                131072
            };
            // This is a real retained Vec allocation, not a synthetic byte credit.
            let retained = QueryArena::<u8>::new(&memory, capacity).unwrap();
            assert!(retained.capacity() >= capacity);
            assert!(retained.reserved_bytes() >= retained.capacity());
            let query_base = memory.reserved_bytes();
            let aggregate_base = shared.reserved_bytes().unwrap();
            let bytes = vec![b'x'; 131072];
            let source = Source(ResultInput {
                view: context.view(),
                rows: 0,
                outcome: Outcome::Read,
                pools: Pools {
                    bytes: &bytes,
                    ..Pools::default()
                },
            });
            let result = PreparedGraphResult::copy_from(&source, &mut context);
            if shared_limit {
                assert!(matches!(
                    result,
                    Err(CompletedError::Runtime(RuntimeError::Memory(
                        MemoryError::Store(_)
                    )))
                ));
            } else {
                assert!(matches!(
                    result,
                    Err(CompletedError::Runtime(RuntimeError::Memory(
                        MemoryError::Limit
                    )))
                ));
            }
            assert_eq!(memory.reserved_bytes(), query_base);
            assert_eq!(shared.reserved_bytes().unwrap(), aggregate_base);
            assert_eq!(context.counters().get(WorkKind::CopiedBytes), 0);
        }
        assert_eq!(shared.reserved_bytes().unwrap(), store_base);
    }
}
