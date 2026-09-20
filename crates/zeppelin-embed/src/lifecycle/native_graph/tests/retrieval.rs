#![cfg(test)]

use super::*;
use crate::ingest::{DocId, DocumentVersion, Revision};
use crate::property_graph::query::eligibility::{Eligibility, EligibleNodeSet};
use crate::property_graph::query::plan::SearchMode;
use crate::property_graph::query::runtime::{RetainedView, RuntimeError, WorkKind};
use crate::property_graph::retrieval::{
    IdentityAdapterError, NativeRetrievalContext, PreparedEligibility, RetrievalError,
    document_version_for_node, node_version_from_document, staged_node_version,
};

#[test]
fn ze60_full_width_identity_and_staged_node_versions() {
    let low = 0xfeed_beef_u128;
    let first = NodeId::new((1_u128 << 100) | low).unwrap();
    let second = NodeId::new((2_u128 << 100) | low).unwrap();
    let revision = GraphRevision::new(u64::MAX).unwrap();
    let first_version = document_version_for_node(first, revision);
    let second_version = document_version_for_node(second, revision);
    assert_ne!(first_version, second_version);
    assert_eq!(first_version.doc_id().get(), first.get());
    assert_eq!(first_version.revision().get(), u64::MAX);
    assert_eq!(
        node_version_from_document(first_version).unwrap(),
        (first, revision)
    );

    let maximum = NodeId::new(u128::MAX).unwrap();
    assert_eq!(
        node_version_from_document(document_version_for_node(maximum, revision)).unwrap(),
        (maximum, revision)
    );
    assert_eq!(
        node_version_from_document(DocumentVersion::new(DocId::new(0), Revision::new(1))),
        Err(IdentityAdapterError::ZeroNode)
    );
    assert_eq!(
        node_version_from_document(DocumentVersion::new(DocId::new(1), Revision::new(0))),
        Err(IdentityAdapterError::ZeroRevision)
    );

    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let identity = StoreInstanceId::new(1_u128 << 87).unwrap();
    let base = EmptyProducerBase {
        identity: BaseIdentity {
            store: identity,
            generation: GraphGeneration::new(0),
            roots: None,
        },
        high_waters: StageHighWaters {
            node: 1_u128 << 100,
            relationship: 1_u128 << 110,
            ..StageHighWaters::default()
        },
        document: None,
    };
    let node_contents = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    with_local_refs(|refs| {
        let node = StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "node").unwrap(),
            revision: GraphRevision::new(17).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&node_contents)),
        };
        let relationship = StructuredWrite {
            key: ApplicationKey::new(EntityKind::Relationship, "app", "self").unwrap(),
            revision: GraphRevision::new(23).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Relationship {
                source: NodeRef::Local(refs.node(0).unwrap()),
                target: NodeRef::Local(refs.node(0).unwrap()),
                relationship_type: GraphName::new("SELF").unwrap(),
                properties: &[],
            }),
        };
        let staged =
            stage_structured(&base, &[node, relationship], &writer, &mut |_| Ok(())).unwrap();
        let node_delta = staged.deltas().first().unwrap();
        let relationship_delta = staged.deltas().get(1).unwrap();
        let fields = node_delta.provenance().fields();
        let EntityId::Node(node_id) = fields.incarnation else {
            panic!("staged node changed identity domain");
        };
        assert_eq!(
            staged_node_version(node_delta).unwrap(),
            document_version_for_node(node_id, fields.installed_revision)
        );
        assert_eq!(
            staged_node_version(relationship_delta),
            Err(IdentityAdapterError::Relationship)
        );
    });
    store.close().unwrap();
}

struct PreparationChecks<'a> {
    query: &'a [f32],
}

impl NativeReadConsumer<()> for PreparationChecks<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), TreeError> {
        let context = NativeRetrievalContext::new(view, runtime)
            .map_err(|_| TreeError::Invalid("retrieval preparation context"))?;
        for mode in [
            SearchMode::Default,
            SearchMode::Auto,
            SearchMode::Exact,
            SearchMode::Scan,
        ] {
            let prepared = context
                .prepare_vector(self.query, mode, Eligibility::AllIndexed, runtime)
                .map_err(|_| TreeError::Invalid("valid retrieval vector"))?;
            assert!(std::ptr::eq(prepared.coordinates(), self.query));
            assert_eq!(prepared.mode(), mode);
            assert!(matches!(
                prepared.eligibility(),
                PreparedEligibility::AllIndexed
            ));
        }
        assert!(matches!(
            context.prepare_vector(
                &self.query[..self.query.len() - 1],
                SearchMode::Exact,
                Eligibility::AllIndexed,
                runtime
            ),
            Err(RetrievalError::Dimension { .. })
        ));
        let mut invalid = self.query.to_vec();
        invalid[0] = f32::NAN;
        assert!(matches!(
            context.prepare_vector(
                &invalid,
                SearchMode::Exact,
                Eligibility::AllIndexed,
                runtime
            ),
            Err(RetrievalError::Vector(
                crate::quant::QuantError::NonFinite { index: 0 }
            ))
        ));
        invalid[0] = f32::INFINITY;
        assert!(matches!(
            context.prepare_vector(
                &invalid,
                SearchMode::Exact,
                Eligibility::AllIndexed,
                runtime
            ),
            Err(RetrievalError::Vector(
                crate::quant::QuantError::NonFinite { index: 0 }
            ))
        ));
        let empty = EligibleNodeSet::build(runtime, 0, []).unwrap();
        let prepared = context
            .prepare_vector(
                self.query,
                SearchMode::Exact,
                Eligibility::Set(&empty),
                runtime,
            )
            .map_err(|_| TreeError::Invalid("explicit empty retrieval eligibility"))?;
        assert!(matches!(
            prepared.eligibility(),
            PreparedEligibility::Set(ids) if ids.is_empty()
        ));
        Ok(())
    }
}

struct ExpectedPreparationFailure<'a> {
    query: &'a [f32],
    expected: fn(&RetrievalError) -> bool,
    cancel: Option<CancelToken>,
}

impl NativeReadConsumer<(u64, u64)> for ExpectedPreparationFailure<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(u64, u64), TreeError> {
        let context = NativeRetrievalContext::new(view, runtime)
            .map_err(|_| TreeError::Invalid("retrieval refusal context"))?;
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        let failure = match context.prepare_vector(
            self.query,
            SearchMode::Exact,
            Eligibility::AllIndexed,
            runtime,
        ) {
            Ok(_) => panic!("preparation must refuse"),
            Err(error) => error,
        };
        assert!((self.expected)(&failure), "unexpected failure: {failure:?}");
        Ok((
            runtime.counters().get(WorkKind::VectorCoordinates),
            runtime.counters().get(WorkKind::VectorBytes),
        ))
    }
}

#[test]
fn ze60_preparation_reuses_bounded_vector_validation() {
    use crate::lifecycle::prepared::{VectorValidationError, validate_vector_coordinates};

    assert_eq!(
        validate_vector_coordinates(&[], 65_536, |_| Ok::<(), u8>(())),
        Err(VectorValidationError::Data(
            crate::quant::QuantError::EmptyVector
        ))
    );
    assert_eq!(
        validate_vector_coordinates(&[0.0, 1.0], 1, |_| Ok::<(), u8>(())),
        Err(VectorValidationError::Data(
            crate::quant::QuantError::DimensionTooLarge {
                actual: 2,
                maximum: 1
            }
        ))
    );
    assert_eq!(
        validate_vector_coordinates(&[0.0, f32::NAN], 2, |_| Ok::<(), u8>(())),
        Err(VectorValidationError::Data(
            crate::quant::QuantError::NonFinite { index: 1 }
        ))
    );
    assert_eq!(
        validate_vector_coordinates(&[0.0, 1.0], 2, |index| {
            if index == 1 { Err(7_u8) } else { Ok(()) }
        }),
        Err(VectorValidationError::Control(7))
    );

    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let identity = StoreInstanceId::new(1_u128 << 84).unwrap();
    store
        .install_native_graph_for_test(actual_producer_bundle_with_dimensions(
            &store,
            directory.path(),
            identity,
            0,
            true,
            0,
            false,
            2,
        ))
        .unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    store
        .with_native_read(
            &control,
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            16,
            PreparationChecks {
                query: &[0.5, -0.5],
            },
        )
        .unwrap();

    let limited = RuntimeLimits::default()
        .with_limit(WorkKind::VectorCoordinates, 1)
        .unwrap()
        .with_limit(WorkKind::VectorBytes, 4)
        .unwrap();
    let counts = store
        .with_native_read(
            &control,
            limited,
            8 * 1024 * 1024,
            16,
            ExpectedPreparationFailure {
                query: &[0.5, -0.5],
                expected: |error| {
                    matches!(
                        error,
                        RetrievalError::Control(RuntimeError::Limit(WorkKind::VectorCoordinates))
                    )
                },
                cancel: None,
            },
        )
        .unwrap();
    assert_eq!(counts, (1, 4));

    let cancel = CancelToken::new();
    let cancelled_control = QueryControl::Cancel(cancel.clone());
    let cancelled = store.with_native_read(
        &cancelled_control,
        RuntimeLimits::default(),
        8 * 1024 * 1024,
        16,
        ExpectedPreparationFailure {
            query: &[0.5, -0.5],
            expected: |error| {
                matches!(
                    error,
                    RetrievalError::Storage(TreeError::Runtime(RuntimeError::Value(
                        crate::property_graph::query::QueryError::Cancelled
                    )))
                )
            },
            cancel: Some(cancel),
        },
    );
    assert!(
        cancelled.is_err(),
        "terminal callback checkpoint preserves cancellation"
    );
    drop(store);

    let absent_directory = tempfile::tempdir().unwrap();
    let absent = Store::open(
        absent_directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let absent_identity = StoreInstanceId::new((1_u128 << 84) + 1).unwrap();
    absent
        .install_native_graph_for_test(actual_producer_bundle(
            &absent,
            absent_directory.path(),
            absent_identity,
        ))
        .unwrap();
    absent
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            16,
            ExpectedPreparationFailure {
                query: &[0.5, -0.5],
                expected: |error| matches!(error, RetrievalError::NoVectorSpace),
                cancel: None,
            },
        )
        .unwrap();
    absent.close().unwrap();

    let wide_directory = tempfile::tempdir().unwrap();
    let wide = Store::open(
        wide_directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let wide_identity = StoreInstanceId::new((1_u128 << 84) + 2).unwrap();
    let wide_query = vec![0.25_f32; crate::kernels::MAX_DOT_I8_DIMENSION + 1];
    wide.install_native_graph_for_test(actual_producer_bundle_with_dimensions(
        &wide,
        wide_directory.path(),
        wide_identity,
        0,
        true,
        0,
        false,
        wide_query.len(),
    ))
    .unwrap();
    wide.with_native_read(
        &QueryControl::Cancel(CancelToken::new()),
        RuntimeLimits::default(),
        8 * 1024 * 1024,
        16,
        PreparationChecks { query: &wide_query },
    )
    .unwrap();
    wide.close().unwrap();
}

#[test]
fn ze60_rejects_foreign_eligibility_and_runtime_before_reads() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let identity = StoreInstanceId::new(1_u128 << 83).unwrap();
    store
        .install_native_graph_for_test(actual_producer_bundle_with_dimensions(
            &store,
            directory.path(),
            identity,
            0,
            true,
            0,
            false,
            2,
        ))
        .unwrap();
    let first = store.admit_native_read().unwrap();
    let second = store.admit_native_read().unwrap();
    assert_eq!(first.bundle().base(), second.bundle().base());
    assert!(!std::ptr::eq(first.query_view(), second.query_view()));
    let shared = GraphResources::from_store(&store).unwrap();
    let first_memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
    let second_memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut first_runtime =
        RuntimeContext::new(&first, &control, &first_memory, RuntimeLimits::default()).unwrap();
    let first_capability = NativeReadCapability::admit(&first, &first_runtime).unwrap();
    let mut first_initial = TreeResources::for_query(&mut first_runtime).unwrap();
    let first_source = NativeQuerySource::new(first_capability, &first_initial, 16).unwrap();
    let first_catalog = NativeCatalog::open(&first_source, &mut first_initial).unwrap();
    drop(first_initial);
    let first_view = GraphReadView::new(&first_source, &first_catalog).unwrap();
    let context = NativeRetrievalContext::new(&first_view, &mut first_runtime).unwrap();

    let mut second_runtime =
        RuntimeContext::new(&second, &control, &second_memory, RuntimeLimits::default()).unwrap();
    let node = NodeId::new((1_u128 << 100) + 1).unwrap();
    let matching =
        EligibleNodeSet::build(&mut first_runtime, 1, [first.query_view().node(node)]).unwrap();
    let foreign =
        EligibleNodeSet::build(&mut second_runtime, 1, [second.query_view().node(node)]).unwrap();

    let prepared = context
        .prepare_vector(
            &[0.5, -0.5],
            SearchMode::Exact,
            Eligibility::Set(&matching),
            &mut first_runtime,
        )
        .unwrap();
    assert!(matches!(
        prepared.eligibility(),
        PreparedEligibility::Set(ids) if *ids == [node]
    ));
    let omitted = context
        .prepare_vector(
            &[0.5, -0.5],
            SearchMode::Exact,
            Eligibility::AllIndexed,
            &mut first_runtime,
        )
        .unwrap();
    assert!(matches!(
        omitted.eligibility(),
        PreparedEligibility::AllIndexed
    ));
    let empty = EligibleNodeSet::build(&mut first_runtime, 0, []).unwrap();
    let explicit_empty = context
        .prepare_vector(
            &[0.5, -0.5],
            SearchMode::Exact,
            Eligibility::Set(&empty),
            &mut first_runtime,
        )
        .unwrap();
    assert!(matches!(
        explicit_empty.eligibility(),
        PreparedEligibility::Set(ids) if ids.is_empty()
    ));

    let before = first_runtime.counters();
    assert!(matches!(
        context.prepare_vector(
            &[0.5, -0.5],
            SearchMode::Exact,
            Eligibility::Set(&foreign),
            &mut first_runtime
        ),
        Err(RetrievalError::Eligibility(
            crate::property_graph::query::QueryError::ForeignView
        ))
    ));
    let after = first_runtime.counters();
    for kind in [
        WorkKind::Lookups,
        WorkKind::VectorCoordinates,
        WorkKind::VectorBytes,
    ] {
        assert_eq!(before.get(kind), after.get(kind));
    }

    let mut other_runtime =
        RuntimeContext::new(&first, &control, &first_memory, RuntimeLimits::default()).unwrap();
    assert!(matches!(
        context.prepare_vector(
            &[0.5, -0.5],
            SearchMode::Exact,
            Eligibility::AllIndexed,
            &mut other_runtime
        ),
        Err(RetrievalError::Storage(TreeError::Invalid(
            "foreign native retrieval runtime"
        )))
    ));
    assert_eq!(other_runtime.counters().get(WorkKind::VectorCoordinates), 0);

    drop(other_runtime);
    drop(context);
    drop(first_view);
    drop(first_catalog);
    drop(first_source);
    drop(first_runtime);
    drop(second_runtime);
    drop(first);
    drop(second);
    store.close().unwrap();
}

#[test]
fn ze60_resolves_version_and_payload_from_retained_native_view() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let identity = StoreInstanceId::new(1_u128 << 82).unwrap();
    let node_a = NodeId::new((1_u128 << 100) + 1).unwrap();
    let node_b = NodeId::new((1_u128 << 100) + 2).unwrap();
    let installed = actual_producer_bundle_with_dimensions(
        &store,
        directory.path(),
        identity,
        0,
        true,
        0,
        false,
        2,
    );
    let replacement = tombstoned_endpoint_bundle(&store, directory.path(), &installed, node_b);
    store.install_native_graph_for_test(installed).unwrap();

    let old = store.admit_native_read().unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let old_memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut old_runtime =
        RuntimeContext::new(&old, &control, &old_memory, RuntimeLimits::default()).unwrap();
    let old_capability = NativeReadCapability::admit(&old, &old_runtime).unwrap();
    let mut old_initial = TreeResources::for_query(&mut old_runtime).unwrap();
    let old_source = NativeQuerySource::new(old_capability, &old_initial, 16).unwrap();
    let old_catalog = NativeCatalog::open(&old_source, &mut old_initial).unwrap();
    drop(old_initial);
    let old_view = GraphReadView::new(&old_source, &old_catalog).unwrap();
    let old_retrieval = NativeRetrievalContext::new(&old_view, &mut old_runtime).unwrap();

    let version_a = document_version_for_node(node_a, GraphRevision::new(1).unwrap());
    let version_b = document_version_for_node(node_b, GraphRevision::new(1).unwrap());
    let resolved_a = old_retrieval.resolve(version_a, &mut old_runtime).unwrap();
    assert_eq!(resolved_a.version(), version_a);
    assert!(
        resolved_a
            .node()
            .record()
            .canonical()
            .stored_vector()
            .is_none()
    );
    let empty_text = old_retrieval
        .copy_text(&resolved_a, &mut old_runtime)
        .unwrap()
        .expect("present empty text");
    assert!(empty_text.is_empty());
    assert!(
        old_retrieval
            .copy_vector(&resolved_a, &mut old_runtime)
            .unwrap()
            .is_none()
    );

    let resolved_b = old_retrieval.resolve(version_b, &mut old_runtime).unwrap();
    assert!(
        old_retrieval
            .copy_text(&resolved_b, &mut old_runtime)
            .unwrap()
            .is_none()
    );
    let copied_before = old_runtime.counters().get(WorkKind::CopiedBytes);
    let vector = old_retrieval
        .copy_vector(&resolved_b, &mut old_runtime)
        .unwrap()
        .expect("present vector");
    assert_eq!(
        vector
            .as_slice()
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>(),
        [0x3f80_0001, 0x8000_0000]
    );
    assert_eq!(
        old_runtime.counters().get(WorkKind::CopiedBytes) - copied_before,
        16,
        "two source-to-field and two field-to-output f32 copies"
    );
    drop(vector);

    assert!(matches!(
        old_retrieval.resolve(
            document_version_for_node(node_b, GraphRevision::new(2).unwrap()),
            &mut old_runtime
        ),
        Err(RetrievalError::Version(_))
    ));
    let absent = NodeId::new((1_u128 << 100) + 99).unwrap();
    let absent_version = document_version_for_node(absent, GraphRevision::new(1).unwrap());
    assert!(matches!(
        old_retrieval.resolve(absent_version, &mut old_runtime),
        Err(RetrievalError::MissingVersion(version)) if version == absent_version
    ));

    store.install_native_graph_for_test(replacement).unwrap();
    let retained_b = old_retrieval.resolve(version_b, &mut old_runtime).unwrap();
    let retained_vector = old_retrieval
        .copy_vector(&retained_b, &mut old_runtime)
        .unwrap()
        .unwrap();
    assert_eq!(retained_vector.as_slice()[0].to_bits(), 0x3f80_0001);
    drop(retained_vector);

    let current = store.admit_native_read().unwrap();
    let current_memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
    let mut current_runtime = RuntimeContext::new(
        &current,
        &control,
        &current_memory,
        RuntimeLimits::default(),
    )
    .unwrap();
    let current_capability = NativeReadCapability::admit(&current, &current_runtime).unwrap();
    let mut current_initial = TreeResources::for_query(&mut current_runtime).unwrap();
    let current_source = NativeQuerySource::new(current_capability, &current_initial, 16).unwrap();
    let current_catalog = NativeCatalog::open(&current_source, &mut current_initial).unwrap();
    drop(current_initial);
    let current_view = GraphReadView::new(&current_source, &current_catalog).unwrap();
    let current_retrieval =
        NativeRetrievalContext::new(&current_view, &mut current_runtime).unwrap();
    assert!(matches!(
        current_retrieval.resolve(version_b, &mut current_runtime),
        Err(RetrievalError::MissingVersion(version)) if version == version_b
    ));

    drop(current_retrieval);
    drop(current_view);
    drop(current_catalog);
    drop(current_source);
    drop(current_runtime);
    drop(current);
    drop(old_retrieval);
    drop(old_view);
    drop(old_catalog);
    drop(old_source);
    drop(old_runtime);
    drop(old);
    store.close().unwrap();

    let graph_directory = tempfile::tempdir().unwrap();
    let graph_store = Store::open(
        graph_directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let graph_identity = StoreInstanceId::new((1_u128 << 82) + 1).unwrap();
    graph_store
        .install_native_graph_for_test(actual_producer_bundle(
            &graph_store,
            graph_directory.path(),
            graph_identity,
        ))
        .unwrap();
    struct GraphOnly(NodeId);
    impl NativeReadConsumer<(bool, bool)> for GraphOnly {
        fn consume<'s, 'lease, 'm, 'g>(
            &mut self,
            view: &GraphReadView<'s, 'lease, 'm, 'g>,
            runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        ) -> Result<(bool, bool), TreeError> {
            let retrieval = NativeRetrievalContext::new(view, runtime)
                .map_err(|_| TreeError::Invalid("graph-only retrieval"))?;
            let version = document_version_for_node(self.0, GraphRevision::new(1).unwrap());
            let resolved = retrieval
                .resolve(version, runtime)
                .map_err(|_| TreeError::Invalid("graph-only resolve"))?;
            Ok((
                retrieval
                    .copy_text(&resolved, runtime)
                    .map_err(|_| TreeError::Invalid("graph-only text"))?
                    .is_none(),
                retrieval
                    .copy_vector(&resolved, runtime)
                    .map_err(|_| TreeError::Invalid("graph-only vector"))?
                    .is_none(),
            ))
        }
    }
    assert_eq!(
        graph_store
            .with_native_read(
                &QueryControl::Cancel(CancelToken::new()),
                RuntimeLimits::default(),
                8 * 1024 * 1024,
                16,
                GraphOnly(node_b),
            )
            .unwrap(),
        (true, true)
    );
    graph_store.close().unwrap();
}

struct CopyVectorOut(NodeId);

impl NativeReadConsumer<Vec<u32>> for CopyVectorOut {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Vec<u32>, TreeError> {
        let retrieval = NativeRetrievalContext::new(view, runtime)
            .map_err(|_| TreeError::Invalid("copy-out retrieval"))?;
        let version = document_version_for_node(self.0, GraphRevision::new(1).unwrap());
        let resolved = retrieval
            .resolve(version, runtime)
            .map_err(|_| TreeError::Invalid("copy-out resolve"))?;
        let vector = retrieval
            .copy_vector(&resolved, runtime)
            .map_err(|_| TreeError::Invalid("copy-out vector"))?
            .ok_or(TreeError::Invalid("copy-out vector absent"))?;
        Ok(vector
            .as_slice()
            .iter()
            .map(|value| value.to_bits())
            .collect())
    }
}

struct CloseFirstProbe {
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
    observed: std::sync::mpsc::Sender<Option<String>>,
    cancel: CancelToken,
}

impl NativeReadConsumer<()> for CloseFirstProbe {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), TreeError> {
        let retrieval = NativeRetrievalContext::new(view, runtime)
            .map_err(|_| TreeError::Invalid("close-first retrieval"))?;
        self.entered
            .send(())
            .map_err(|_| TreeError::Invalid("close-first entered channel"))?;
        self.release
            .recv()
            .map_err(|_| TreeError::Invalid("close-first release channel"))?;
        self.cancel.cancel();
        let close_first = match retrieval.prepare_vector(
            &[0.5, -0.5],
            SearchMode::Exact,
            Eligibility::AllIndexed,
            runtime,
        ) {
            Err(RetrievalError::Storage(TreeError::Runtime(RuntimeError::Value(
                crate::property_graph::query::QueryError::ReadCancelled,
            )))) => None,
            Err(error) => Some(format!("{error:?}")),
            Ok(_) => Some("unexpected successful prepared vector".to_owned()),
        };
        self.observed
            .send(close_first)
            .map_err(|_| TreeError::Invalid("close-first result channel"))?;
        Err(TreeError::Invalid(
            "close-first callback returns no payload",
        ))
    }
}

#[test]
fn ze60_native_adapter_refusal_releases_charges_and_leases() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let identity = StoreInstanceId::new(1_u128 << 81).unwrap();
    let node = NodeId::new((1_u128 << 100) + 2).unwrap();
    store
        .install_native_graph_for_test(actual_producer_bundle_with_dimensions(
            &store,
            directory.path(),
            identity,
            0,
            true,
            0,
            false,
            2,
        ))
        .unwrap();
    let baseline = store.stats().unwrap();
    let copied = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            16,
            CopyVectorOut(node),
        )
        .unwrap();
    assert_eq!(copied, [0x3f80_0001, 0x8000_0000]);
    let after_copy = store.stats().unwrap();
    assert_eq!(after_copy.active_queries, baseline.active_queries);
    assert_eq!(after_copy.mapped_bytes, baseline.mapped_bytes);
    assert_eq!(
        after_copy.resident_owned_bytes,
        baseline.resident_owned_bytes
    );

    let lease = store.admit_native_read().unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
    let memory_baseline = memory.reserved_bytes();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut runtime =
        RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
    let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
    let mut initial = TreeResources::for_query(&mut runtime).unwrap();
    let source = NativeQuerySource::new(capability, &initial, 16).unwrap();
    let catalog = NativeCatalog::open(&source, &mut initial).unwrap();
    drop(initial);
    let view = GraphReadView::new(&source, &catalog).unwrap();
    let source_steady = memory.reserved_bytes();
    let source_peak = memory.peak_reserved_bytes();
    let retrieval = NativeRetrievalContext::new(&view, &mut runtime).unwrap();
    let retrieval_charge = memory.reserved_bytes() - source_steady;
    assert!(retrieval_charge > 0);
    drop(retrieval);
    assert_eq!(memory.reserved_bytes(), source_steady);
    drop(view);
    drop(catalog);
    drop(source);
    drop(runtime);
    assert_eq!(memory.reserved_bytes(), memory_baseline);

    let tight = QueryMemory::new(&shared, source_peak).unwrap();
    let mut tight_runtime =
        RuntimeContext::new(&lease, &control, &tight, RuntimeLimits::default()).unwrap();
    let capability = NativeReadCapability::admit(&lease, &tight_runtime).unwrap();
    let mut tight_initial = TreeResources::for_query(&mut tight_runtime).unwrap();
    let tight_source = NativeQuerySource::new(capability, &tight_initial, 16).unwrap();
    let tight_catalog = NativeCatalog::open(&tight_source, &mut tight_initial).unwrap();
    drop(tight_initial);
    let tight_view = GraphReadView::new(&tight_source, &tight_catalog).unwrap();
    let tight_steady = tight.reserved_bytes();
    let available = source_peak - tight_steady;
    let filler = if available >= retrieval_charge {
        Some(tight.reserve(available - retrieval_charge + 1).unwrap())
    } else {
        None
    };
    let refused_steady = tight.reserved_bytes();
    assert!(matches!(
        NativeRetrievalContext::new(&tight_view, &mut tight_runtime),
        Err(RetrievalError::Control(RuntimeError::Memory(_)))
    ));
    assert_eq!(tight.reserved_bytes(), refused_steady);
    drop(filler);
    assert_eq!(tight.reserved_bytes(), tight_steady);
    drop(tight_view);
    drop(tight_catalog);
    drop(tight_source);
    drop(tight_runtime);
    assert_eq!(
        tight.reserved_bytes(),
        std::mem::size_of::<QueryMemory<'_>>()
    );

    let deadline_clock = Arc::new(ManualMonotonicClock::new());
    let deadline =
        Deadline::after_with_test_clock(Duration::from_secs(1), deadline_clock.clone()).unwrap();
    let deadline_control = QueryControl::Deadline(deadline);
    let deadline_memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
    let mut deadline_runtime = RuntimeContext::new(
        &lease,
        &deadline_control,
        &deadline_memory,
        RuntimeLimits::default(),
    )
    .unwrap();
    let capability = NativeReadCapability::admit(&lease, &deadline_runtime).unwrap();
    let mut deadline_initial = TreeResources::for_query(&mut deadline_runtime).unwrap();
    let deadline_source = NativeQuerySource::new(capability, &deadline_initial, 16).unwrap();
    let deadline_catalog = NativeCatalog::open(&deadline_source, &mut deadline_initial).unwrap();
    drop(deadline_initial);
    let deadline_view = GraphReadView::new(&deadline_source, &deadline_catalog).unwrap();
    let deadline_retrieval =
        NativeRetrievalContext::new(&deadline_view, &mut deadline_runtime).unwrap();
    deadline_clock.advance(Duration::from_secs(2));
    assert!(matches!(
        deadline_retrieval.prepare_vector(
            &[0.5, -0.5],
            SearchMode::Exact,
            Eligibility::AllIndexed,
            &mut deadline_runtime
        ),
        Err(RetrievalError::Storage(TreeError::Runtime(
            RuntimeError::Value(crate::property_graph::query::QueryError::Timeout)
        )))
    ));
    drop(deadline_retrieval);
    drop(deadline_view);
    drop(deadline_catalog);
    drop(deadline_source);
    drop(deadline_runtime);
    assert_eq!(
        deadline_memory.reserved_bytes(),
        std::mem::size_of::<QueryMemory<'_>>()
    );
    drop(lease);
    store.close().unwrap();

    let close_directory = tempfile::tempdir().unwrap();
    let close_store = Arc::new(
        Store::open(
            close_directory.path(),
            OpenOptions::new()
                .with_max_resident_bytes(256 * 1024 * 1024)
                .with_reader_drain_timeout(Duration::ZERO),
        )
        .unwrap(),
    );
    let close_identity = StoreInstanceId::new((1_u128 << 81) + 1).unwrap();
    close_store
        .install_native_graph_for_test(actual_producer_bundle_with_dimensions(
            &close_store,
            close_directory.path(),
            close_identity,
            0,
            true,
            0,
            false,
            2,
        ))
        .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (observed_tx, observed_rx) = mpsc::channel();
    let caller_cancel = CancelToken::new();
    let read_control = QueryControl::Cancel(caller_cancel.clone());
    let reading = Arc::clone(&close_store);
    let reader = std::thread::spawn(move || {
        reading.with_native_read(
            &read_control,
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            16,
            CloseFirstProbe {
                entered: entered_tx,
                release: release_rx,
                observed: observed_tx,
                cancel: caller_cancel,
            },
        )
    });
    entered_rx.recv().unwrap();
    let witness = close_store.admit_native_read().unwrap();
    let publication = Arc::clone(&close_store.native_graph);
    let closing = Arc::clone(&close_store);
    let (closed_tx, closed_rx) = mpsc::channel();
    let closer = std::thread::spawn(move || {
        closed_tx.send(closing.close()).unwrap();
    });
    let mut state = publication.state.lock().unwrap();
    while witness.check_active().is_ok() {
        state = publication.changed.wait(state).unwrap();
    }
    drop(state);
    assert!(closed_rx.try_recv().is_err());
    drop(witness);
    release_tx.send(()).unwrap();
    let observed = observed_rx.recv().unwrap();
    assert!(
        observed.is_none(),
        "unexpected close-first result: {observed:?}"
    );
    assert!(reader.join().unwrap().is_err());
    closed_rx.recv().unwrap().unwrap();
    closer.join().unwrap();
}
