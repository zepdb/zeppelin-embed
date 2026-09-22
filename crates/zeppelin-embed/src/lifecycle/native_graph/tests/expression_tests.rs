use super::*;
use crate::property_graph::RelId;
use crate::property_graph::query::expression::{
    ExpressionCapacity, ExpressionFailure, NativeExpressionEvaluator,
};
use crate::property_graph::query::plan::{
    AggregateExpression, BinaryExpression, ExprId, Expression, Literal, NodeFacts, Operator,
    OperatorKind, Parameter, ParameterBinding, ParameterId, PlanBacking, PlanDescription,
    PlanFootprint, PlanNodeId, Projection, RetainedRegion, SlotId, UnaryExpression,
    VALIDATION_SCRATCH_BYTES, ValueKinds,
};
use crate::property_graph::query::relational::Schema;
use crate::property_graph::query::resources::{
    QueryArena, QueryInputs, RetainedAllocation, RetentionInventory,
};
use crate::property_graph::query::runtime::{ArenaCapacity, RowBatch, RuntimeContext, WorkKind};
use crate::property_graph::query::{
    Arithmetic, Comparison, QueryList, QueryValue, StringPredicate,
};

fn expression_producer_bundle_with_extra_nodes(
    store: &Store,
    directory: &Path,
    identity: StoreInstanceId,
    extra_nodes: usize,
    with_vector: bool,
    extra_relationships: usize,
    max_ids: bool,
) -> NativeGraphBundleInput {
    let shared = GraphResources::from_store(store).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let storage = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let base = BaseIdentity {
        store: identity,
        generation: GraphGeneration::new(0),
        roots: None,
    };
    let document = with_vector.then(|| EmbeddingTower {
        model_id: "ze45-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x45, 0xa5],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    });
    let node_count = 2_u128 + extra_nodes as u128;
    let relationship_count = 3_u128 + extra_relationships as u128;
    let admitted = EmptyProducerBase {
        identity: base,
        high_waters: StageHighWaters {
            node: if max_ids {
                u128::MAX - node_count
            } else {
                1_u128 << 100
            },
            relationship: if max_ids {
                u128::MAX - relationship_count
            } else {
                1_u128 << 110
            },
            ..StageHighWaters::default()
        },
        document: document.clone(),
    };
    let label = GraphName::new("Label").unwrap();
    let rel_type = GraphName::new("R").unwrap();
    let alternate_rel_type = GraphName::new("S").unwrap();
    let namespace = "app";
    let extra_keys = (0..extra_nodes)
        .map(|index| format!("bulk-{index:04}"))
        .collect::<Vec<_>>();
    let extra_relationship_keys = (0..extra_relationships)
        .map(|index| format!("bulk-rel-{index:04}"))
        .collect::<Vec<_>>();
    let multi_chunk_text = "text-猫"
        .repeat(crate::property_graph::storage::payload::CHUNK_BYTES / "text-猫".len() + 17);
    with_local_refs(|refs| {
        let mut labels_a = [label];
        let string_values = ["猫", ""];
        let bool_values = [true, false];
        let integer_values = [i64::MIN, 17];
        let float_values = [f64::from_bits(0x8000_0000_0000_0000), 1.5];
        let mut node_properties = [
            GraphProperty::new(
                GraphName::new("string").unwrap(),
                PropertyValue::new(PropertyData::String("猫\0zeppelin")).unwrap(),
            ),
            GraphProperty::new(
                GraphName::new("bool").unwrap(),
                PropertyValue::new(PropertyData::Bool(true)).unwrap(),
            ),
            GraphProperty::new(
                GraphName::new("integer").unwrap(),
                PropertyValue::new(PropertyData::I64(i64::MIN)).unwrap(),
            ),
            GraphProperty::new(
                GraphName::new("float").unwrap(),
                PropertyValue::new(PropertyData::F64(f64::from_bits(0x8000_0000_0000_0000)))
                    .unwrap(),
            ),
            GraphProperty::new(
                GraphName::new("empty").unwrap(),
                PropertyValue::new(PropertyData::EmptyList { count: 0 }).unwrap(),
            ),
            GraphProperty::new(
                GraphName::new("strings").unwrap(),
                PropertyValue::new(PropertyData::Strings(&string_values)).unwrap(),
            ),
            GraphProperty::new(
                GraphName::new("bools").unwrap(),
                PropertyValue::new(PropertyData::Bools(&bool_values)).unwrap(),
            ),
            GraphProperty::new(
                GraphName::new("integers").unwrap(),
                PropertyValue::new(PropertyData::Integers(&integer_values)).unwrap(),
            ),
            GraphProperty::new(
                GraphName::new("floats").unwrap(),
                PropertyValue::new(PropertyData::Floats(&float_values)).unwrap(),
            ),
        ];
        let relationship_properties = [GraphProperty::new(
            GraphName::new("weight").unwrap(),
            PropertyValue::new(PropertyData::I64(-17)).unwrap(),
        )];
        let coordinates = [f32::from_bits(0x3f80_0001), f32::from_bits(0x8000_0000)];
        let embedding = document.as_ref().map(|document| {
            crate::property_graph::CanonicalEmbedding::new(document, &coordinates).unwrap()
        });
        let node_a = CanonicalContents::node(
            &mut labels_a,
            &mut node_properties,
            Some(multi_chunk_text.as_str()),
            None,
        )
        .unwrap();
        let node_b = CanonicalContents::node(&mut [], &mut [], Some(""), embedding).unwrap();
        let node_zero_term = CanonicalContents::node(&mut [], &mut [], Some("!!!"), None).unwrap();
        let node_absent = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let mut requests = Vec::with_capacity(5 + extra_nodes + extra_relationships);
        requests.push(StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, namespace, "a").unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&node_a)),
        });
        requests.push(StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, namespace, "b").unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&node_b)),
        });
        for (index, key) in extra_keys.iter().enumerate() {
            requests.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, namespace, key).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(if index == 0 {
                    &node_zero_term
                } else {
                    &node_absent
                })),
            });
        }
        requests.push(StructuredWrite {
            key: ApplicationKey::new(EntityKind::Relationship, namespace, "ab1").unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Relationship {
                source: NodeRef::Local(refs.node(0).unwrap()),
                target: NodeRef::Local(refs.node(1).unwrap()),
                relationship_type: rel_type,
                properties: &[],
            }),
        });
        for key in &extra_relationship_keys {
            requests.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, namespace, key).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: rel_type,
                    properties: &[],
                }),
            });
        }
        requests.push(StructuredWrite {
            key: ApplicationKey::new(EntityKind::Relationship, namespace, "self").unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Relationship {
                source: NodeRef::Local(refs.node(0).unwrap()),
                target: NodeRef::Local(refs.node(0).unwrap()),
                relationship_type: alternate_rel_type,
                properties: &[],
            }),
        });
        requests.push(StructuredWrite {
            key: ApplicationKey::new(EntityKind::Relationship, namespace, "ab2").unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Relationship {
                source: NodeRef::Local(refs.node(0).unwrap()),
                target: NodeRef::Local(refs.node(1).unwrap()),
                relationship_type: rel_type,
                properties: &relationship_properties,
            }),
        });
        let staged = stage_structured(&admitted, &requests, &writer, &mut |_| Ok(())).unwrap();
        let target_generation = GraphGeneration::new(1);
        let mut next_artifact = 1_000_u128;
        let mut next_serial = 1_u64;
        let mut resources = TreeResources::for_prepare(&storage, u64::MAX).unwrap();
        let mut objects = PreparedObjects::new(
            &MissingProducerSource,
            || {
                let artifact = ArtifactId::new(next_artifact)?;
                let creation_serial = next_serial;
                next_artifact += 1;
                next_serial += 1;
                Ok(ArtifactIdentity {
                    store: identity,
                    artifact,
                    generation: target_generation,
                    creation_serial,
                })
            },
            identity,
            target_generation,
            PackLimits {
                artifact_bytes: crate::property_graph::storage::artifact::MAX_ARTIFACT_BYTES,
                blocks: 8_192,
            },
            &storage,
            &mut resources,
        )
        .unwrap();
        let base_catalog_artifact = ArtifactId::new(999).unwrap();
        let committed = crate::property_graph::wal::CommitState {
            store: identity,
            generation: GraphGeneration::new(0),
            sequence: 40,
            graph: WalGraphRoots::default(),
            catalog: required(
                identity,
                GraphGeneration::new(0),
                base_catalog_artifact.get(),
                BlockKind::CommitParticipant,
                ContainerKind::Object,
            ),
            vector: None,
            text: None,
            reclaim: None,
            high_waters: HighWaters::default(),
            prepared_inventories: crate::property_graph::wal::ReferenceList::Values(&[]),
        };
        let candidate = prepare_native_graph(
            &mut objects,
            &staged,
            NativeGraphBase {
                directories: DirectoryBase {
                    identity: base,
                    roots: GraphRoots::from_references(
                        identity,
                        GraphGeneration::new(0),
                        [None; 8],
                    )
                    .unwrap(),
                },
                committed,
            },
            &EmptyPreparationCatalog(base),
            document.as_ref(),
            &storage,
            &mut resources,
        )
        .unwrap_or_else(|error| {
            panic!(
                "actual producer preparation failed: {error}; current={} peak={}",
                storage.reserved_bytes(),
                storage.peak_reserved_bytes()
            )
        });
        let roots = candidate.roots();
        let sequence = candidate.sequence();
        objects.finish(&mut resources).unwrap();
        let mut descriptors = Vec::new();
        for index in 0..objects.len() {
            let object = objects.artifact(index).unwrap();
            let bytes = object.bytes();
            let checksum =
                u64::from_le_bytes(*bytes[bytes.len() - 8..].first_chunk::<8>().unwrap());
            descriptors.push(ArtifactDescriptor {
                store: identity,
                artifact: object.identity().artifact,
                generation: object.identity().generation,
                serial: object.identity().creation_serial,
                bytes: bytes.len() as u32,
                family: 17,
                version: 1,
                checksum,
            });
            std::fs::write(artifact_path(directory, object.identity().artifact), bytes).unwrap();
        }
        let mut wal_roots = WalGraphRoots::default();
        for (slot, reference) in roots.references().into_iter().enumerate() {
            if let Some(block) = reference {
                let descriptor = *descriptors
                    .iter()
                    .find(|descriptor| descriptor.artifact == block.artifact)
                    .unwrap();
                wal_roots.slots[slot] = Some(RequiredRef {
                    object: descriptor,
                    block,
                });
            }
        }
        let catalog_identity = ArtifactIdentity {
            store: identity,
            artifact: ArtifactId::new(8_001).unwrap(),
            generation: target_generation,
            creation_serial: next_serial,
        };
        next_serial += 1;
        let catalog =
            write_catalog_with_symbols(directory, catalog_identity, &staged, document.as_ref());
        let root_identity = ArtifactIdentity {
            store: identity,
            artifact: ArtifactId::new(8_002).unwrap(),
            generation: target_generation,
            creation_serial: next_serial,
        };
        let root_envelope = write_framed_file(
            directory,
            ContainerKind::RootEnvelope,
            root_identity,
            &[Block {
                kind: BlockKind::CheckpointPayload,
                payload: b"controlled-fixture-root",
            }],
        );
        NativeGraphBundleInput {
            base: BaseIdentity {
                store: identity,
                generation: target_generation,
                roots: Some(root_identity.artifact),
            },
            root_envelope,
            roots,
            wal_roots,
            sequence,
            catalog,
            vector: None,
            text: None,
            reclaim: None,
            high_waters: HighWaters {
                node: staged.high_waters().node,
                relationship: staged.high_waters().relationship,
                symbols: [
                    staged.high_waters().symbols.label,
                    staged.high_waters().symbols.relationship_type,
                    staged.high_waters().symbols.property,
                    staged.high_waters().symbols.namespace,
                ],
                creation_serial: next_serial,
            },
            prepared_inventories: Vec::new(),
            lexical: TokenizerEpoch::of(&TokenizerConfig::text_default()),
            document: document.clone(),
        }
    })
}

struct ScalarOwnershipTracer {
    exact_expression_limit: bool,
    close_first: Option<(
        Arc<Store>,
        CancelToken,
        Arc<std::sync::Mutex<Option<std::thread::JoinHandle<Result<(), StoreError>>>>>,
    )>,
}

impl NativeReadConsumer<()> for ScalarOwnershipTracer {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), TreeError> {
        let parameter_name = String::from("increment");
        let binding_name = String::from("increment");
        let text_parameter_name = String::from("text");
        let text_binding_name = String::from("text");
        let list_parameter_name = String::from("items");
        let list_binding_name = String::from("items");
        let parameter_text = String::from("owned 猫");
        let parameter_list_text = String::from("nested 犬");
        let parameter_list_values = [
            QueryValue::I64(7),
            QueryValue::String(parameter_list_text.as_str()),
        ];
        let parameter_list = QueryList::new(&parameter_list_values, runtime.values())
            .map_err(crate::property_graph::query::runtime::RuntimeError::Value)
            .map_err(TreeError::Runtime)?;
        let parameters = [
            Parameter {
                name: parameter_name.as_str(),
                kinds: ValueKinds::I64,
            },
            Parameter {
                name: text_parameter_name.as_str(),
                kinds: ValueKinds::STRING,
            },
            Parameter {
                name: list_parameter_name.as_str(),
                kinds: ValueKinds::LIST,
            },
        ];
        let bindings = [
            ParameterBinding {
                name: binding_name.as_str(),
                value: QueryValue::I64(41),
            },
            ParameterBinding {
                name: text_binding_name.as_str(),
                value: QueryValue::String(parameter_text.as_str()),
            },
            ParameterBinding {
                name: list_binding_name.as_str(),
                value: QueryValue::List(parameter_list),
            },
        ];
        let unicode_literal = String::from("é猫x");
        let prefix_literal = String::from("é猫");
        let list_items = [ExprId(1), ExprId(14), ExprId(4)];
        let nested_items = [ExprId(20), ExprId(3)];
        let expressions = [
            Expression::Parameter(ParameterId(0)),
            Expression::Literal(Literal::I64(1)),
            Expression::Binary {
                operation: BinaryExpression::Arithmetic(Arithmetic::Add),
                left: ExprId(0),
                right: ExprId(1),
            },
            Expression::Literal(Literal::Bool(true)),
            Expression::Literal(Literal::Null),
            Expression::Binary {
                operation: BinaryExpression::And,
                left: ExprId(3),
                right: ExprId(4),
            },
            Expression::Binary {
                operation: BinaryExpression::Or,
                left: ExprId(3),
                right: ExprId(4),
            },
            Expression::Binary {
                operation: BinaryExpression::Xor,
                left: ExprId(3),
                right: ExprId(4),
            },
            Expression::Unary {
                operation: UnaryExpression::Not,
                operand: ExprId(4),
            },
            Expression::Literal(Literal::I64(i64::MIN)),
            Expression::Unary {
                operation: UnaryExpression::Negate,
                operand: ExprId(9),
            },
            Expression::Unary {
                operation: UnaryExpression::Positive,
                operand: ExprId(1),
            },
            Expression::Literal(Literal::F64(f64::from_bits(0x4009_21fb_5444_2d18))),
            Expression::Binary {
                operation: BinaryExpression::Comparison(Comparison::Equal),
                left: ExprId(12),
                right: ExprId(12),
            },
            Expression::Literal(Literal::String(unicode_literal.as_str())),
            Expression::Unary {
                operation: UnaryExpression::Size,
                operand: ExprId(14),
            },
            Expression::Literal(Literal::String(prefix_literal.as_str())),
            Expression::Binary {
                operation: BinaryExpression::String(StringPredicate::StartsWith),
                left: ExprId(14),
                right: ExprId(16),
            },
            Expression::Binary {
                operation: BinaryExpression::String(StringPredicate::EndsWith),
                left: ExprId(14),
                right: ExprId(14),
            },
            Expression::Binary {
                operation: BinaryExpression::String(StringPredicate::Contains),
                left: ExprId(14),
                right: ExprId(16),
            },
            Expression::List(&list_items),
            Expression::Literal(Literal::I64(-2)),
            Expression::Binary {
                operation: BinaryExpression::Index,
                left: ExprId(20),
                right: ExprId(21),
            },
            Expression::Binary {
                operation: BinaryExpression::In,
                left: ExprId(1),
                right: ExprId(20),
            },
            Expression::List(&nested_items),
            Expression::Unary {
                operation: UnaryExpression::Size,
                operand: ExprId(24),
            },
            Expression::Parameter(ParameterId(1)),
            Expression::Parameter(ParameterId(2)),
            Expression::Unary {
                operation: UnaryExpression::IsNull,
                operand: ExprId(4),
            },
            Expression::Unary {
                operation: UnaryExpression::IsNotNull,
                operand: ExprId(4),
            },
            Expression::Literal(Literal::I64(6)),
            Expression::Binary {
                operation: BinaryExpression::Arithmetic(Arithmetic::Subtract),
                left: ExprId(30),
                right: ExprId(1),
            },
            Expression::Binary {
                operation: BinaryExpression::Arithmetic(Arithmetic::Multiply),
                left: ExprId(30),
                right: ExprId(1),
            },
            Expression::Binary {
                operation: BinaryExpression::Arithmetic(Arithmetic::Divide),
                left: ExprId(30),
                right: ExprId(1),
            },
            Expression::Binary {
                operation: BinaryExpression::Arithmetic(Arithmetic::Remainder),
                left: ExprId(30),
                right: ExprId(1),
            },
            Expression::Literal(Literal::I64(9_007_199_254_740_993)),
            Expression::Literal(Literal::F64(9_007_199_254_740_992.0)),
            Expression::Binary {
                operation: BinaryExpression::Comparison(Comparison::Equal),
                left: ExprId(35),
                right: ExprId(36),
            },
            Expression::Binary {
                operation: BinaryExpression::Comparison(Comparison::Greater),
                left: ExprId(35),
                right: ExprId(36),
            },
            Expression::Aggregate {
                operation: AggregateExpression::Count { distinct: false },
                operand: None,
            },
        ];
        let scalar_projections: [Projection; 39] = std::array::from_fn(|index| Projection {
            slot: SlotId(100 + index as u32),
            expression: ExprId(index as u32),
        });
        let aggregate_projection = [Projection {
            slot: SlotId(200),
            expression: ExprId(39),
        }];
        let aggregate_input = [PlanNodeId(0)];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &aggregate_input,
                kind: OperatorKind::Aggregate {
                    keys: &scalar_projections,
                    aggregates: &aggregate_projection,
                },
            },
        ];
        let mut facts = QueryArena::new(runtime.memory(), operators.len())
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        facts
            .push(NodeFacts::default())
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        facts
            .push(NodeFacts::default())
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        let parameter_region =
            RetainedRegion::declared(parameter_name.as_ptr() as usize, parameter_name.capacity())
                .unwrap();
        let mut regions = vec![
            RetainedRegion::slice(&operators).unwrap(),
            RetainedRegion::slice(&aggregate_input).unwrap(),
            RetainedRegion::slice(&scalar_projections).unwrap(),
            RetainedRegion::slice(&aggregate_projection).unwrap(),
            RetainedRegion::slice(&expressions).unwrap(),
            RetainedRegion::slice(&parameters).unwrap(),
            parameter_region,
            RetainedRegion::declared(
                text_parameter_name.as_ptr() as usize,
                text_parameter_name.capacity(),
            )
            .unwrap(),
            RetainedRegion::declared(
                list_parameter_name.as_ptr() as usize,
                list_parameter_name.capacity(),
            )
            .unwrap(),
            RetainedRegion::declared(
                unicode_literal.as_ptr() as usize,
                unicode_literal.capacity(),
            )
            .unwrap(),
            RetainedRegion::declared(prefix_literal.as_ptr() as usize, prefix_literal.capacity())
                .unwrap(),
            RetainedRegion::slice(&list_items).unwrap(),
            RetainedRegion::slice(&nested_items).unwrap(),
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .unwrap(),
        ];
        regions.sort();
        let external_bytes = regions
            .iter()
            .map(|region| region.end() - region.start())
            .sum::<usize>()
            + regions.capacity() * std::mem::size_of::<RetainedRegion>()
            + std::mem::size_of::<PlanDescription<'_>>()
            + VALIDATION_SCRATCH_BYTES;
        let mut plan_capacity = runtime
            .memory()
            .reserve_external_capacity()
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        plan_capacity
            .reserve_additional(external_bytes)
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        let description = PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &parameters,
            root: PlanNodeId(1),
            eager_searches: &[],
        };
        let footprint = PlanFootprint::declared(runtime.memory().reserved_bytes());
        let (plan, facts_owner) = facts
            .validate_plan(
                description,
                footprint,
                PlanBacking::new(
                    &regions,
                    regions.capacity() * std::mem::size_of::<RetainedRegion>(),
                )
                .unwrap(),
                runtime.values(),
            )
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        let owners = vec![
            RetainedAllocation::array(&operators).unwrap(),
            RetainedAllocation::array(&aggregate_input).unwrap(),
            RetainedAllocation::array(&scalar_projections).unwrap(),
            RetainedAllocation::array(&aggregate_projection).unwrap(),
            RetainedAllocation::array(&expressions).unwrap(),
            RetainedAllocation::array(&parameters).unwrap(),
            RetainedAllocation::string(&parameter_name).unwrap(),
            RetainedAllocation::string(&text_parameter_name).unwrap(),
            RetainedAllocation::string(&list_parameter_name).unwrap(),
            RetainedAllocation::string(&unicode_literal).unwrap(),
            RetainedAllocation::string(&prefix_literal).unwrap(),
            RetainedAllocation::array(&list_items).unwrap(),
            RetainedAllocation::array(&nested_items).unwrap(),
            facts_owner,
            RetainedAllocation::array(&bindings).unwrap(),
            RetainedAllocation::string(&binding_name).unwrap(),
            RetainedAllocation::string(&text_binding_name).unwrap(),
            RetainedAllocation::string(&list_binding_name).unwrap(),
            RetainedAllocation::string(&parameter_text).unwrap(),
            RetainedAllocation::string(&parameter_list_text).unwrap(),
            RetainedAllocation::array(&parameter_list_values).unwrap(),
        ];
        let runtime_plan = QueryInputs::reserve(
            runtime.memory(),
            RetentionInventory::vector(&owners)
                .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
                .map_err(TreeError::Runtime)?,
            runtime.values(),
        )
        .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
        .map_err(TreeError::Runtime)?
        .admit_plan(&plan, runtime.values())
        .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
        .map_err(TreeError::Runtime)?;

        let schema = Schema::new(runtime, &[]).map_err(TreeError::Runtime)?;
        let mut input = RowBatch::new(runtime, 0, 1, 0).map_err(TreeError::Runtime)?;
        input.push_row(&[], runtime).map_err(TreeError::Runtime)?;
        let mut evaluator = NativeExpressionEvaluator::new(
            &runtime_plan,
            &bindings,
            ExpressionCapacity {
                cells: 8,
                string_bytes: 64,
            },
            runtime,
        )
        .map_err(|_| TreeError::Invalid("expression constructor"))?;
        if let Some((store, caller_cancel, closer_slot)) = self.close_first.take() {
            let closer = std::thread::spawn(move || store.close());
            *closer_slot.lock().unwrap() = Some(closer);
            loop {
                match view.validate_expression_owner(runtime) {
                    Err(TreeError::Runtime(
                        crate::property_graph::query::runtime::RuntimeError::Value(
                            crate::property_graph::query::QueryError::ReadCancelled,
                        ),
                    )) => break,
                    Ok(()) => std::thread::yield_now(),
                    Err(error) => return Err(error),
                }
            }
            caller_cancel.cancel();
            let error = evaluator
                .evaluate(ExprId(1), &schema, &input, usize::MAX, view, runtime)
                .unwrap_err();
            assert!(matches!(
                error.failure,
                ExpressionFailure::Tree(TreeError::Runtime(
                    crate::property_graph::query::runtime::RuntimeError::Value(
                        crate::property_graph::query::QueryError::ReadCancelled
                    )
                ))
            ));
            return Ok(());
        }
        if self.exact_expression_limit {
            for expected in 1..=2 {
                assert!(matches!(
                    evaluator.evaluate(ExprId(1), &schema, &input, 0, view, runtime),
                    Ok(QueryValue::I64(1))
                ));
                assert_eq!(runtime.counters().get(WorkKind::Expressions), expected);
            }
            let destination = RowBatch::new(runtime, 1, 1, 8).map_err(TreeError::Runtime)?;
            let error = evaluator
                .evaluate(ExprId(1), &schema, &input, 0, view, runtime)
                .unwrap_err();
            assert!(matches!(
                error.failure,
                ExpressionFailure::Runtime(
                    crate::property_graph::query::runtime::RuntimeError::Limit(
                        WorkKind::Expressions
                    )
                )
            ));
            assert_eq!(runtime.counters().get(WorkKind::Expressions), 2);
            assert_eq!(destination.rows(), 0);
            return Ok(());
        }
        let value = evaluator
            .evaluate(ExprId(2), &schema, &input, 0, view, runtime)
            .map_err(|_| TreeError::Invalid("expression evaluation"))?;
        let mut output = RowBatch::new(runtime, 1, 1, 8).map_err(TreeError::Runtime)?;
        output
            .push_row(&[value], runtime)
            .map_err(TreeError::Runtime)?;
        assert!(matches!(output.value(0, 0), Some(QueryValue::I64(42))));
        assert!(matches!(
            evaluator.evaluate(ExprId(5), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Null)
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(6), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Bool(true))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(7), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Null)
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(8), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Null)
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(10), &schema, &input, 0, view, runtime),
            Err(error) if matches!(error.failure, ExpressionFailure::Runtime(
                crate::property_graph::query::runtime::RuntimeError::Value(
                    crate::property_graph::query::QueryError::ArithmeticOverflow
                )
            ))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(11), &schema, &input, 0, view, runtime),
            Ok(QueryValue::I64(1))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(13), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Bool(true))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(15), &schema, &input, 0, view, runtime),
            Ok(QueryValue::I64(3))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(17), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Bool(true))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(18), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Bool(true))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(19), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Bool(true))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(22), &schema, &input, 0, view, runtime),
            Ok(QueryValue::String("é猫x"))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(23), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Bool(true))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(25), &schema, &input, 0, view, runtime),
            Ok(QueryValue::I64(2))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(26), &schema, &input, 0, view, runtime),
            Ok(QueryValue::String("owned 猫"))
        ));
        let copied_parameter_list = evaluator
            .evaluate(ExprId(27), &schema, &input, 0, view, runtime)
            .unwrap();
        let QueryValue::List(copied_parameter_list) = copied_parameter_list else {
            panic!("parameter list must remain a list");
        };
        assert_eq!(copied_parameter_list.len(), 2);
        assert!(matches!(
            copied_parameter_list.get(1),
            Some(QueryValue::String("nested 犬"))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(28), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Bool(true))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(29), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Bool(false))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(31), &schema, &input, 0, view, runtime),
            Ok(QueryValue::I64(5))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(32), &schema, &input, 0, view, runtime),
            Ok(QueryValue::I64(6))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(33), &schema, &input, 0, view, runtime),
            Ok(QueryValue::I64(6))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(34), &schema, &input, 0, view, runtime),
            Ok(QueryValue::I64(0))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(37), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Bool(false))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(38), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Bool(true))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(39), &schema, &input, 0, view, runtime),
            Err(error) if matches!(error.failure, ExpressionFailure::Plan(
                crate::property_graph::query::plan::PlanError::Aggregate
            ))
        ));
        assert_eq!(runtime.counters().get(WorkKind::Expressions), 76);
        Ok(())
    }
}

#[test]
fn native_expression_scalar_dispatch_and_parameters() {
    let directory = tempfile::tempdir().expect("store directory");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .expect("open store");
    let identity = StoreInstanceId::new(1_u128 << 86).unwrap();
    store
        .install_native_graph_for_test(actual_producer_bundle(&store, directory.path(), identity))
        .unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    store
        .with_native_read(
            &control,
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            16,
            ScalarOwnershipTracer {
                exact_expression_limit: false,
                close_first: None,
            },
        )
        .expect("admitted expression evaluation");
    store.close().unwrap();
}

struct NativeReadTracer {
    hidden_relationship: bool,
    atomic_failure: bool,
    cancel_after_polls: Option<(usize, CancelToken, ExprId)>,
}

impl NativeReadConsumer<()> for NativeReadTracer {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), TreeError> {
        let property_name = String::from("weight");
        let missing_property_name = String::from("missing");
        let label_name = String::from("Label");
        let unknown_label_name = String::from("Unknown");
        let weight = GraphName::new(property_name.as_str()).unwrap();
        let missing = GraphName::new(missing_property_name.as_str()).unwrap();
        let label = GraphName::new(label_name.as_str()).unwrap();
        let unknown = GraphName::new(unknown_label_name.as_str()).unwrap();
        let stored_property_names = [
            "string", "bool", "integer", "float", "empty", "strings", "bools", "integers", "floats",
        ]
        .map(String::from);
        let stored_property_symbols = stored_property_names
            .each_ref()
            .map(|name| GraphName::new(name).unwrap());
        let expressions = [
            Expression::Slot(SlotId(11)),
            Expression::Slot(SlotId(29)),
            Expression::Property {
                entity: ExprId(1),
                name: weight,
            },
            Expression::Property {
                entity: ExprId(0),
                name: missing,
            },
            Expression::HasLabel {
                entity: ExprId(0),
                label,
            },
            Expression::HasLabel {
                entity: ExprId(0),
                label: unknown,
            },
            Expression::Unary {
                operation: UnaryExpression::Labels,
                operand: ExprId(0),
            },
            Expression::Unary {
                operation: UnaryExpression::RelType,
                operand: ExprId(1),
            },
            Expression::Unary {
                operation: UnaryExpression::StoredText,
                operand: ExprId(0),
            },
            Expression::Unary {
                operation: UnaryExpression::NodeIdText,
                operand: ExprId(0),
            },
            Expression::Unary {
                operation: UnaryExpression::RelIdText,
                operand: ExprId(1),
            },
            Expression::Literal(Literal::Null),
            Expression::Property {
                entity: ExprId(11),
                name: weight,
            },
            Expression::HasLabel {
                entity: ExprId(11),
                label,
            },
            Expression::Unary {
                operation: UnaryExpression::StoredText,
                operand: ExprId(11),
            },
            Expression::Property {
                entity: ExprId(0),
                name: stored_property_symbols[0],
            },
            Expression::Property {
                entity: ExprId(0),
                name: stored_property_symbols[1],
            },
            Expression::Property {
                entity: ExprId(0),
                name: stored_property_symbols[2],
            },
            Expression::Property {
                entity: ExprId(0),
                name: stored_property_symbols[3],
            },
            Expression::Property {
                entity: ExprId(0),
                name: stored_property_symbols[4],
            },
            Expression::Property {
                entity: ExprId(0),
                name: stored_property_symbols[5],
            },
            Expression::Property {
                entity: ExprId(0),
                name: stored_property_symbols[6],
            },
            Expression::Property {
                entity: ExprId(0),
                name: stored_property_symbols[7],
            },
            Expression::Property {
                entity: ExprId(0),
                name: stored_property_symbols[8],
            },
            Expression::Slot(SlotId(12)),
            Expression::Unary {
                operation: UnaryExpression::StoredText,
                operand: ExprId(24),
            },
            Expression::Slot(SlotId(13)),
            Expression::Unary {
                operation: UnaryExpression::StoredText,
                operand: ExprId(26),
            },
            Expression::Slot(SlotId(14)),
            Expression::Unary {
                operation: UnaryExpression::StoredText,
                operand: ExprId(28),
            },
            Expression::Slot(SlotId(30)),
            Expression::Unary {
                operation: UnaryExpression::RelIdText,
                operand: ExprId(30),
            },
        ];
        let projections: [Projection; 32] = std::array::from_fn(|index| Projection {
            slot: SlotId(100 + index as u32),
            expression: ExprId(index as u32),
        });
        let input_1 = [PlanNodeId(0)];
        let input_2 = [PlanNodeId(1)];
        let input_3 = [PlanNodeId(2)];
        let input_4 = [PlanNodeId(3)];
        let input_5 = [PlanNodeId(4)];
        let input_6 = [PlanNodeId(5)];
        let project_input = [PlanNodeId(6)];
        let node = NodeId::new((1_u128 << 100) + 1).unwrap();
        let node_b = NodeId::new((1_u128 << 100) + 2).unwrap();
        let node_c = NodeId::new((1_u128 << 100) + 3).unwrap();
        let node_d = NodeId::new((1_u128 << 100) + 4).unwrap();
        let relationship_one = RelId::new((1_u128 << 110) + 1).unwrap();
        let relationship = RelId::new((1_u128 << 110) + 3).unwrap();
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &input_1,
                kind: OperatorKind::LookupNode {
                    output: SlotId(11),
                    id: node,
                },
            },
            Operator {
                inputs: &input_2,
                kind: OperatorKind::LookupNode {
                    output: SlotId(12),
                    id: node_b,
                },
            },
            Operator {
                inputs: &input_3,
                kind: OperatorKind::LookupNode {
                    output: SlotId(13),
                    id: node_c,
                },
            },
            Operator {
                inputs: &input_4,
                kind: OperatorKind::LookupNode {
                    output: SlotId(14),
                    id: node_d,
                },
            },
            Operator {
                inputs: &input_5,
                kind: OperatorKind::LookupRelationship {
                    output: SlotId(30),
                    id: relationship_one,
                },
            },
            Operator {
                inputs: &input_6,
                kind: OperatorKind::LookupRelationship {
                    output: SlotId(29),
                    id: relationship,
                },
            },
            Operator {
                inputs: &project_input,
                kind: OperatorKind::Project(&projections),
            },
        ];
        let mut facts = QueryArena::new(runtime.memory(), operators.len())
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        for _ in &operators {
            facts
                .push(NodeFacts::default())
                .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
                .map_err(TreeError::Runtime)?;
        }
        let mut regions = vec![
            RetainedRegion::slice(&operators).unwrap(),
            RetainedRegion::slice(&input_1).unwrap(),
            RetainedRegion::slice(&input_2).unwrap(),
            RetainedRegion::slice(&input_3).unwrap(),
            RetainedRegion::slice(&input_4).unwrap(),
            RetainedRegion::slice(&input_5).unwrap(),
            RetainedRegion::slice(&input_6).unwrap(),
            RetainedRegion::slice(&project_input).unwrap(),
            RetainedRegion::slice(&projections).unwrap(),
            RetainedRegion::slice(&expressions).unwrap(),
            RetainedRegion::declared(property_name.as_ptr() as usize, property_name.capacity())
                .unwrap(),
            RetainedRegion::declared(
                missing_property_name.as_ptr() as usize,
                missing_property_name.capacity(),
            )
            .unwrap(),
            RetainedRegion::declared(label_name.as_ptr() as usize, label_name.capacity()).unwrap(),
            RetainedRegion::declared(
                unknown_label_name.as_ptr() as usize,
                unknown_label_name.capacity(),
            )
            .unwrap(),
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .unwrap(),
        ];
        regions.extend(stored_property_names.iter().map(|name| {
            RetainedRegion::declared(name.as_ptr() as usize, name.capacity()).unwrap()
        }));
        regions.sort();
        let region_bytes = regions.capacity() * std::mem::size_of::<RetainedRegion>();
        let external_bytes = regions
            .iter()
            .map(|region| region.end() - region.start())
            .sum::<usize>()
            + region_bytes
            + std::mem::size_of::<PlanDescription<'_>>()
            + VALIDATION_SCRATCH_BYTES;
        let mut plan_capacity = runtime
            .memory()
            .reserve_external_capacity()
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        plan_capacity
            .reserve_additional(external_bytes)
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        let description = PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(7),
            eager_searches: &[],
        };
        let footprint = PlanFootprint::declared(runtime.memory().reserved_bytes());
        let (plan, facts_owner) = facts
            .validate_plan(
                description,
                footprint,
                PlanBacking::new(&regions, region_bytes).unwrap(),
                runtime.values(),
            )
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        let mut owners = vec![
            RetainedAllocation::array(&operators).unwrap(),
            RetainedAllocation::array(&input_1).unwrap(),
            RetainedAllocation::array(&input_2).unwrap(),
            RetainedAllocation::array(&input_3).unwrap(),
            RetainedAllocation::array(&input_4).unwrap(),
            RetainedAllocation::array(&input_5).unwrap(),
            RetainedAllocation::array(&input_6).unwrap(),
            RetainedAllocation::array(&project_input).unwrap(),
            RetainedAllocation::array(&projections).unwrap(),
            RetainedAllocation::array(&expressions).unwrap(),
            RetainedAllocation::string(&property_name).unwrap(),
            RetainedAllocation::string(&missing_property_name).unwrap(),
            RetainedAllocation::string(&label_name).unwrap(),
            RetainedAllocation::string(&unknown_label_name).unwrap(),
            facts_owner,
        ];
        owners.extend(
            stored_property_names
                .iter()
                .map(|name| RetainedAllocation::string(name).unwrap()),
        );
        let runtime_plan = QueryInputs::reserve(
            runtime.memory(),
            RetentionInventory::vector(&owners)
                .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
                .map_err(TreeError::Runtime)?,
            runtime.values(),
        )
        .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
        .map_err(TreeError::Runtime)?
        .admit_plan(&plan, runtime.values())
        .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
        .map_err(TreeError::Runtime)?;
        let slots = [
            SlotId(11),
            SlotId(12),
            SlotId(13),
            SlotId(14),
            SlotId(29),
            SlotId(30),
        ];
        let schema = Schema::new(runtime, &slots).map_err(TreeError::Runtime)?;
        let mut input = RowBatch::new(runtime, 6, 2, 192).map_err(TreeError::Runtime)?;
        input
            .push_row(
                &[
                    runtime.view().node(node),
                    runtime.view().node(node_b),
                    runtime.view().node(node_c),
                    runtime.view().node(node_d),
                    runtime.view().relationship(relationship),
                    runtime.view().relationship(relationship_one),
                ],
                runtime,
            )
            .map_err(TreeError::Runtime)?;
        let missing_node = NodeId::new(node.get() + 99).unwrap();
        input
            .push_row(
                &[
                    runtime.view().node(missing_node),
                    runtime.view().node(node_b),
                    runtime.view().node(node_c),
                    runtime.view().node(node_d),
                    runtime.view().relationship(relationship),
                    runtime.view().relationship(relationship_one),
                ],
                runtime,
            )
            .map_err(TreeError::Runtime)?;
        let mut evaluator = NativeExpressionEvaluator::new(
            &runtime_plan,
            &[],
            ExpressionCapacity {
                cells: 128,
                string_bytes: 2 * crate::property_graph::storage::payload::CHUNK_BYTES,
            },
            runtime,
        )
        .map_err(|_| TreeError::Invalid("native expression constructor"))?;

        if let Some((polls, cancel, expression)) = self.cancel_after_polls.take() {
            evaluator.cancel_after_scratch_polls(polls, cancel);
            let error = evaluator
                .evaluate(expression, &schema, &input, 0, view, runtime)
                .unwrap_err();
            assert!(matches!(
                error.failure,
                ExpressionFailure::Tree(TreeError::Runtime(
                    crate::property_graph::query::runtime::RuntimeError::Value(
                        crate::property_graph::query::QueryError::Cancelled
                    )
                )) | ExpressionFailure::Runtime(
                    crate::property_graph::query::runtime::RuntimeError::Value(
                        crate::property_graph::query::QueryError::Cancelled
                    )
                )
            ));
            return Ok(());
        }

        if self.atomic_failure {
            let mut destination = RowBatch::new(runtime, 1, 1, 64).map_err(TreeError::Runtime)?;
            let late_error = evaluator
                .evaluate(ExprId(3), &schema, &input, 1, view, runtime)
                .unwrap_err();
            assert!(matches!(
                late_error.failure,
                ExpressionFailure::Tree(TreeError::Invalid("expression node is absent or deleted"))
            ));
            assert_eq!(destination.rows(), 0);
            let recovered = evaluator
                .evaluate(ExprId(16), &schema, &input, 0, view, runtime)
                .unwrap();
            destination
                .push_row(&[recovered], runtime)
                .map_err(TreeError::Runtime)?;
            assert!(matches!(
                destination.value(0, 0),
                Some(QueryValue::Bool(true))
            ));

            let baseline = runtime.memory().reserved_bytes();
            let refused = match NativeExpressionEvaluator::new(
                &runtime_plan,
                &[],
                ExpressionCapacity {
                    cells: usize::MAX,
                    string_bytes: usize::MAX,
                },
                runtime,
            ) {
                Ok(_) => return Err(TreeError::Invalid("oversized expression arena admitted")),
                Err(error) => error,
            };
            assert!(matches!(
                refused,
                ExpressionFailure::Runtime(
                    crate::property_graph::query::runtime::RuntimeError::Memory(_)
                )
            ));
            assert_eq!(runtime.memory().reserved_bytes(), baseline);

            let mut undersized = NativeExpressionEvaluator::new(
                &runtime_plan,
                &[],
                ExpressionCapacity {
                    cells: 4,
                    string_bytes: 32,
                },
                runtime,
            )
            .map_err(|_| TreeError::Invalid("undersized expression constructor"))?;
            let oversize_error = undersized
                .evaluate(ExprId(8), &schema, &input, 0, view, runtime)
                .unwrap_err();
            assert!(matches!(
                oversize_error.failure,
                ExpressionFailure::Runtime(
                    crate::property_graph::query::runtime::RuntimeError::Batch
                )
            ));
            assert_eq!(destination.rows(), 1);
            return Ok(());
        }

        if self.hidden_relationship {
            let error = evaluator
                .evaluate(ExprId(2), &schema, &input, 0, view, runtime)
                .unwrap_err();
            assert!(matches!(
                error.failure,
                ExpressionFailure::Tree(TreeError::Invalid(
                    "expression relationship is absent, deleted, or hidden"
                ))
            ));
            return Ok(());
        }

        assert!(matches!(
            evaluator.evaluate(ExprId(2), &schema, &input, 0, view, runtime),
            Ok(QueryValue::I64(-17))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(3), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Null)
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(4), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Bool(true))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(5), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Bool(false))
        ));
        let labels = evaluator
            .evaluate(ExprId(6), &schema, &input, 0, view, runtime)
            .unwrap();
        let QueryValue::List(labels) = labels else {
            panic!("labels must be a list");
        };
        assert_eq!(labels.len(), 1);
        assert!(matches!(labels.get(0), Some(QueryValue::String("Label"))));
        assert!(matches!(
            evaluator.evaluate(ExprId(7), &schema, &input, 0, view, runtime),
            Ok(QueryValue::String("R"))
        ));
        let text = evaluator
            .evaluate(ExprId(8), &schema, &input, 0, view, runtime)
            .unwrap();
        let QueryValue::String(text) = text else {
            panic!("stored text must be text");
        };
        assert!(text.len() > crate::property_graph::storage::payload::CHUNK_BYTES);
        assert!(text.starts_with("text-猫"));
        let node_text = evaluator
            .evaluate(ExprId(9), &schema, &input, 0, view, runtime)
            .unwrap();
        assert!(matches!(
            node_text,
            QueryValue::String("00000010000000000000000000000001")
        ));
        let mut copied = RowBatch::with_arenas(
            runtime,
            1,
            1,
            32,
            ArenaCapacity {
                string_bytes: 32,
                ..ArenaCapacity::default()
            },
        )
        .map_err(TreeError::Runtime)?;
        copied
            .push_row(&[node_text], runtime)
            .map_err(TreeError::Runtime)?;
        assert!(matches!(
            evaluator.evaluate(ExprId(10), &schema, &input, 0, view, runtime),
            Ok(QueryValue::String("00004000000000000000000000000003"))
        ));
        assert!(matches!(
            copied.value(0, 0),
            Some(QueryValue::String("00000010000000000000000000000001"))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(12), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Null)
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(13), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Null)
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(14), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Null)
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(15), &schema, &input, 0, view, runtime),
            Ok(QueryValue::String("猫\0zeppelin"))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(16), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Bool(true))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(17), &schema, &input, 0, view, runtime),
            Ok(QueryValue::I64(i64::MIN))
        ));
        let scalar_float = evaluator
            .evaluate(ExprId(18), &schema, &input, 0, view, runtime)
            .unwrap();
        assert!(
            matches!(scalar_float, QueryValue::F64(value) if value.to_bits() == 0x8000_0000_0000_0000)
        );
        let empty = evaluator
            .evaluate(ExprId(19), &schema, &input, 0, view, runtime)
            .unwrap();
        assert!(matches!(empty, QueryValue::List(list) if list.is_empty()));
        let strings = evaluator
            .evaluate(ExprId(20), &schema, &input, 0, view, runtime)
            .unwrap();
        let QueryValue::List(strings) = strings else {
            panic!("string property list");
        };
        assert!(matches!(strings.get(0), Some(QueryValue::String("猫"))));
        assert!(matches!(strings.get(1), Some(QueryValue::String(""))));
        let bools = evaluator
            .evaluate(ExprId(21), &schema, &input, 0, view, runtime)
            .unwrap();
        let QueryValue::List(bools) = bools else {
            panic!("bool property list");
        };
        assert!(matches!(bools.get(0), Some(QueryValue::Bool(true))));
        assert!(matches!(bools.get(1), Some(QueryValue::Bool(false))));
        let integers = evaluator
            .evaluate(ExprId(22), &schema, &input, 0, view, runtime)
            .unwrap();
        let QueryValue::List(integers) = integers else {
            panic!("integer property list");
        };
        assert!(matches!(integers.get(0), Some(QueryValue::I64(i64::MIN))));
        assert!(matches!(integers.get(1), Some(QueryValue::I64(17))));
        let floats = evaluator
            .evaluate(ExprId(23), &schema, &input, 0, view, runtime)
            .unwrap();
        let QueryValue::List(floats) = floats else {
            panic!("float property list");
        };
        assert!(
            matches!(floats.get(0), Some(QueryValue::F64(value)) if value.to_bits() == 0x8000_0000_0000_0000)
        );
        assert!(matches!(floats.get(1), Some(QueryValue::F64(1.5))));
        assert!(matches!(
            evaluator.evaluate(ExprId(25), &schema, &input, 0, view, runtime),
            Ok(QueryValue::String(""))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(27), &schema, &input, 0, view, runtime),
            Ok(QueryValue::String("!!!"))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(29), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Null)
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(31), &schema, &input, 0, view, runtime),
            Ok(QueryValue::String("00004000000000000000000000000001"))
        ));
        let error = evaluator
            .evaluate(ExprId(3), &schema, &input, 1, view, runtime)
            .unwrap_err();
        assert!(matches!(
            error.failure,
            ExpressionFailure::Tree(TreeError::Invalid("expression node is absent or deleted"))
        ));
        Ok(())
    }
}

struct ReplaceDuringExpressionRead<'a> {
    store: &'a Store,
    replacement: Option<NativeGraphBundleInput>,
}

impl NativeReadConsumer<()> for ReplaceDuringExpressionRead<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), TreeError> {
        let before = view.sequence();
        self.store
            .install_native_graph_for_test(
                self.replacement
                    .take()
                    .ok_or(TreeError::Invalid("missing expression replacement"))?,
            )
            .map_err(|_| TreeError::Invalid("expression replacement failed"))?;
        if view.sequence() != before {
            return Err(TreeError::Invalid(
                "expression view changed after replacement",
            ));
        }
        NativeReadTracer {
            hidden_relationship: false,
            atomic_failure: false,
            cancel_after_polls: None,
        }
        .consume(view, runtime)
    }
}

#[test]
fn native_expression_reads_real_properties_labels_types_and_text() {
    let directory = tempfile::tempdir().expect("store directory");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .expect("open store");
    let identity = StoreInstanceId::new(1_u128 << 87).unwrap();
    store
        .install_native_graph_for_test(expression_producer_bundle_with_extra_nodes(
            &store,
            directory.path(),
            identity,
            2,
            false,
            0,
            false,
        ))
        .unwrap();
    let installed = current_native_input(&store);
    let control = QueryControl::Cancel(CancelToken::new());
    store
        .with_native_read(
            &control,
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            16,
            NativeReadTracer {
                hidden_relationship: false,
                atomic_failure: false,
                cancel_after_polls: None,
            },
        )
        .expect("native expression reads");
    let hidden = tombstoned_endpoint_bundle(
        &store,
        directory.path(),
        &installed,
        NodeId::new((1_u128 << 100) + 2).unwrap(),
    );
    store.install_native_graph_for_test(hidden).unwrap();
    store
        .with_native_read(
            &control,
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            16,
            NativeReadTracer {
                hidden_relationship: true,
                atomic_failure: false,
                cancel_after_polls: None,
            },
        )
        .expect("hidden endpoint expression rejection");
    store.close().unwrap();
}

#[test]
fn native_expression_preserves_view_and_output_ownership() {
    let directory = tempfile::tempdir().expect("store directory");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .expect("open store");
    let identity = StoreInstanceId::new(1_u128 << 88).unwrap();
    store
        .install_native_graph_for_test(actual_producer_bundle(&store, directory.path(), identity))
        .unwrap();

    let lease = store.admit_native_read().unwrap();
    let foreign_lease = store.admit_native_read().unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let memory = QueryMemory::new(&shared, 16 * 1024 * 1024).unwrap();
    let foreign_memory = QueryMemory::new(&shared, 16 * 1024 * 1024).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut runtime =
        RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
    let capability = NativeReadCapability::admit(&lease, &runtime).unwrap();
    let mut resources = TreeResources::for_query(&mut runtime).unwrap();
    let source = NativeQuerySource::new(capability, &resources, 16).unwrap();
    let catalog = NativeCatalog::open(&source, &mut resources).unwrap();
    drop(resources);
    let view = GraphReadView::new(&source, &catalog).unwrap();

    let literal = String::from("copied 猫 output");
    let expressions = [
        Expression::Literal(Literal::String(literal.as_str())),
        Expression::Literal(Literal::I64(9)),
    ];
    let projections = [
        Projection {
            slot: SlotId(1),
            expression: ExprId(0),
        },
        Projection {
            slot: SlotId(2),
            expression: ExprId(1),
        },
    ];
    let project_input = [PlanNodeId(0)];
    let operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &project_input,
            kind: OperatorKind::Project(&projections),
        },
    ];
    let mut facts = QueryArena::new(&memory, operators.len()).unwrap();
    for _ in &operators {
        facts.push(NodeFacts::default()).unwrap();
    }
    let mut regions = [
        RetainedRegion::slice(&operators).unwrap(),
        RetainedRegion::slice(&project_input).unwrap(),
        RetainedRegion::slice(&projections).unwrap(),
        RetainedRegion::slice(&expressions).unwrap(),
        RetainedRegion::declared(literal.as_ptr() as usize, literal.capacity()).unwrap(),
        RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes()).unwrap(),
    ];
    regions.sort();
    let external_bytes = regions
        .iter()
        .map(|region| region.end() - region.start())
        .sum::<usize>()
        + std::mem::size_of_val(&regions)
        + std::mem::size_of::<PlanDescription<'_>>()
        + VALIDATION_SCRATCH_BYTES;
    let mut plan_capacity = memory.reserve_external_capacity().unwrap();
    plan_capacity.reserve_additional(external_bytes).unwrap();
    let description = PlanDescription {
        operators: &operators,
        expressions: &expressions,
        parameters: &[],
        root: PlanNodeId(1),
        eager_searches: &[],
    };
    let footprint = PlanFootprint::declared(memory.reserved_bytes());
    let (plan, facts_owner) = facts
        .validate_plan(
            description,
            footprint,
            PlanBacking::new(&regions, std::mem::size_of_val(&regions)).unwrap(),
            runtime.values(),
        )
        .unwrap();
    let owners = [
        RetainedAllocation::array(&operators).unwrap(),
        RetainedAllocation::array(&project_input).unwrap(),
        RetainedAllocation::array(&projections).unwrap(),
        RetainedAllocation::array(&expressions).unwrap(),
        RetainedAllocation::string(&literal).unwrap(),
        facts_owner,
    ];
    let runtime_plan = QueryInputs::reserve(
        &memory,
        RetentionInventory::array(&owners),
        runtime.values(),
    )
    .unwrap()
    .admit_plan(&plan, runtime.values())
    .unwrap();
    let schema = Schema::new(&mut runtime, &[]).unwrap();
    let mut input = RowBatch::new(&mut runtime, 0, 1, 0).unwrap();
    input.push_row(&[], &mut runtime).unwrap();
    let mut evaluator = NativeExpressionEvaluator::new(
        &runtime_plan,
        &[],
        ExpressionCapacity {
            cells: 4,
            string_bytes: 64,
        },
        &mut runtime,
    )
    .unwrap();
    let output = evaluator
        .evaluate(ExprId(0), &schema, &input, 0, &view, &mut runtime)
        .unwrap();
    let mut copied = RowBatch::with_arenas(
        &mut runtime,
        1,
        1,
        32,
        ArenaCapacity {
            string_bytes: 64,
            ..ArenaCapacity::default()
        },
    )
    .unwrap();
    copied.push_row(&[output], &mut runtime).unwrap();
    assert!(matches!(
        evaluator.evaluate(ExprId(1), &schema, &input, 0, &view, &mut runtime),
        Ok(QueryValue::I64(9))
    ));
    assert!(matches!(
        copied.value(0, 0),
        Some(QueryValue::String("copied 猫 output"))
    ));

    let mut second_runtime =
        RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default()).unwrap();
    let same_view_error = evaluator
        .evaluate(ExprId(0), &schema, &input, 0, &view, &mut second_runtime)
        .unwrap_err();
    assert!(matches!(
        same_view_error.failure,
        ExpressionFailure::Tree(TreeError::Invalid("native expression owner mismatch"))
    ));

    let mut foreign_memory_runtime =
        RuntimeContext::new(&lease, &control, &foreign_memory, RuntimeLimits::default()).unwrap();
    let foreign_memory_capability =
        NativeReadCapability::admit(&lease, &foreign_memory_runtime).unwrap();
    let mut foreign_memory_resources =
        TreeResources::for_query(&mut foreign_memory_runtime).unwrap();
    let foreign_memory_source =
        NativeQuerySource::new(foreign_memory_capability, &foreign_memory_resources, 16).unwrap();
    let foreign_memory_catalog =
        NativeCatalog::open(&foreign_memory_source, &mut foreign_memory_resources).unwrap();
    drop(foreign_memory_resources);
    let foreign_memory_view =
        GraphReadView::new(&foreign_memory_source, &foreign_memory_catalog).unwrap();
    let foreign_memory_error = evaluator
        .evaluate(
            ExprId(0),
            &schema,
            &input,
            0,
            &foreign_memory_view,
            &mut foreign_memory_runtime,
        )
        .unwrap_err();
    assert!(matches!(
        foreign_memory_error.failure,
        ExpressionFailure::Runtime(crate::property_graph::query::runtime::RuntimeError::Batch)
    ));

    let mut foreign_runtime =
        RuntimeContext::new(&foreign_lease, &control, &memory, RuntimeLimits::default()).unwrap();
    let foreign_capability = NativeReadCapability::admit(&foreign_lease, &foreign_runtime).unwrap();
    let mut foreign_resources = TreeResources::for_query(&mut foreign_runtime).unwrap();
    let foreign_source =
        NativeQuerySource::new(foreign_capability, &foreign_resources, 16).unwrap();
    let foreign_catalog = NativeCatalog::open(&foreign_source, &mut foreign_resources).unwrap();
    drop(foreign_resources);
    let foreign_view = GraphReadView::new(&foreign_source, &foreign_catalog).unwrap();
    let foreign_error = evaluator
        .evaluate(
            ExprId(0),
            &schema,
            &input,
            0,
            &foreign_view,
            &mut foreign_runtime,
        )
        .unwrap_err();
    assert!(matches!(
        foreign_error.failure,
        ExpressionFailure::Runtime(crate::property_graph::query::runtime::RuntimeError::Batch)
    ));

    drop(evaluator);
    drop(copied);
    drop(input);
    drop(runtime_plan);
    drop(plan_capacity);
    drop(plan);
    drop(facts);
    drop(foreign_view);
    drop(foreign_catalog);
    drop(foreign_source);
    drop(foreign_runtime);
    drop(foreign_memory_view);
    drop(foreign_memory_catalog);
    drop(foreign_memory_source);
    drop(foreign_memory_runtime);
    drop(second_runtime);
    drop(view);
    drop(catalog);
    drop(source);
    drop(runtime);
    drop(foreign_lease);
    drop(lease);
    store.close().unwrap();

    let replacement_directory = tempfile::tempdir().expect("replacement store directory");
    let replacement_store = Store::open(
        replacement_directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .expect("open replacement store");
    let replacement_identity = StoreInstanceId::new((1_u128 << 88) + 1).unwrap();
    replacement_store
        .install_native_graph_for_test(expression_producer_bundle_with_extra_nodes(
            &replacement_store,
            replacement_directory.path(),
            replacement_identity,
            2,
            false,
            0,
            false,
        ))
        .unwrap();
    let replacement = bundle(replacement_identity, 2, 901);
    replacement_store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            16,
            ReplaceDuringExpressionRead {
                store: &replacement_store,
                replacement: Some(replacement),
            },
        )
        .expect("old expression property and text after replacement");
    replacement_store.close().unwrap();
}

#[test]
fn native_expression_limits_cancellation_and_failure_are_atomic() {
    let directory = tempfile::tempdir().expect("store directory");
    let store = Arc::new(
        Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("open store"),
    );
    let identity = StoreInstanceId::new(1_u128 << 89).unwrap();
    store
        .install_native_graph_for_test(expression_producer_bundle_with_extra_nodes(
            &store,
            directory.path(),
            identity,
            2,
            false,
            0,
            false,
        ))
        .unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let limits = RuntimeLimits::default()
        .with_limit(WorkKind::Expressions, 2)
        .unwrap();
    store
        .with_native_read(
            &control,
            limits,
            16 * 1024 * 1024,
            16,
            ScalarOwnershipTracer {
                exact_expression_limit: true,
                close_first: None,
            },
        )
        .expect("exact cumulative expression limit");

    store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            16,
            NativeReadTracer {
                hidden_relationship: false,
                atomic_failure: true,
                cancel_after_polls: None,
            },
        )
        .expect("atomic expression failure and owned reservation release");

    let text_cancel = CancelToken::new();
    let text_result = store.with_native_read(
        &QueryControl::Cancel(text_cancel.clone()),
        RuntimeLimits::default(),
        16 * 1024 * 1024,
        16,
        NativeReadTracer {
            hidden_relationship: false,
            atomic_failure: false,
            cancel_after_polls: Some((1, text_cancel, ExprId(8))),
        },
    );
    assert!(matches!(
        text_result,
        Err(NativeGraphError::Read(TreeError::Runtime(
            crate::property_graph::query::runtime::RuntimeError::Value(
                crate::property_graph::query::QueryError::Cancelled
            )
        )))
    ));

    let list_cancel = CancelToken::new();
    let list_result = store.with_native_read(
        &QueryControl::Cancel(list_cancel.clone()),
        RuntimeLimits::default(),
        16 * 1024 * 1024,
        16,
        NativeReadTracer {
            hidden_relationship: false,
            atomic_failure: false,
            cancel_after_polls: Some((0, list_cancel, ExprId(20))),
        },
    );
    assert!(matches!(
        list_result,
        Err(NativeGraphError::Read(TreeError::Runtime(
            crate::property_graph::query::runtime::RuntimeError::Value(
                crate::property_graph::query::QueryError::Cancelled
            )
        )))
    ));

    let close_cancel = CancelToken::new();
    let closer_slot = Arc::new(std::sync::Mutex::new(None));
    let close_result = store.with_native_read(
        &QueryControl::Cancel(close_cancel.clone()),
        RuntimeLimits::default(),
        16 * 1024 * 1024,
        16,
        ScalarOwnershipTracer {
            exact_expression_limit: false,
            close_first: Some((Arc::clone(&store), close_cancel, Arc::clone(&closer_slot))),
        },
    );
    assert!(matches!(
        close_result,
        Err(NativeGraphError::Read(TreeError::Runtime(
            crate::property_graph::query::runtime::RuntimeError::Value(
                crate::property_graph::query::QueryError::ReadCancelled
            )
        )))
    ));
    closer_slot
        .lock()
        .unwrap()
        .take()
        .expect("close thread")
        .join()
        .unwrap()
        .unwrap();
}

// ZE-52 slice C: the overlay-aware expression evaluator entry point. The
// overlay is driven directly here; no mutation executor exists yet.
use crate::property_graph::catalog::SymbolKind;
use crate::property_graph::query::expression::ClauseOverlay;
use crate::property_graph::staging::{
    BatchEntityRef, CanonicalSource, GraphBatchReadView, Membership, WritePhase,
};
use crate::property_graph::{
    CanonicalFingerprint, EntityShape, ExpectedGraphState, GraphDeleteMode, GraphOperation,
    OperationFields, OperationProvenance, RelRef,
};

/// One admitted base holding exactly the fixture's entities. Its property and
/// text answers differ from the installed graph on purpose, so an overlay read
/// that falls through is distinguishable from a `GraphReadView` read.
struct Ze52Base {
    identity: BaseIdentity,
    high_waters: StageHighWaters,
    node_a: NodeId,
    node_b: NodeId,
    relationship: RelId,
    canonical: Vec<u8>,
    fingerprint: CanonicalFingerprint,
    relationship_type: String,
    base_text: String,
}

impl Ze52Base {
    fn new(identity: StoreInstanceId) -> Self {
        let node_a = NodeId::new((1_u128 << 100) + 1).unwrap();
        let node_b = NodeId::new((1_u128 << 100) + 2).unwrap();
        let contents = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let mut canonical = Vec::new();
        contents.write_to(&mut canonical, &mut || Ok(())).unwrap();
        let fingerprint = CanonicalFingerprint::new(
            canonical.len() as u64,
            xxhash_rust::xxh3::xxh3_64(&canonical),
        )
        .unwrap();
        Self {
            identity: BaseIdentity {
                store: identity,
                generation: GraphGeneration::new(0),
                roots: None,
            },
            high_waters: StageHighWaters {
                node: (1_u128 << 100) + 8,
                relationship: (1_u128 << 110) + 8,
                ..StageHighWaters::default()
            },
            node_a,
            node_b,
            relationship: RelId::new((1_u128 << 110) + 1).unwrap(),
            canonical,
            fingerprint,
            relationship_type: String::from("R"),
            base_text: String::from("admitted base text"),
        }
    }

    fn provenance(&self, id: EntityId) -> OperationProvenance<'_> {
        OperationProvenance::from_fields(
            Some(1),
            OperationFields {
                operation: GraphOperation::StructuredCreate,
                key: None,
                requested_revision: GraphRevision::new(1).unwrap(),
                installed_revision: GraphRevision::new(1).unwrap(),
                expected: ExpectedGraphState::Absent,
                incarnation: id,
                delete_mode: None,
                original_generation: self.identity.generation,
            },
        )
        .unwrap()
    }
}

impl CanonicalSource for Ze52Base {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<usize> {
        crate::property_graph::staging::CanonicalSlice(&self.canonical).read_at(offset, output)
    }
}

impl AdmittedBase for Ze52Base {
    fn identity(&self) -> BaseIdentity {
        self.identity
    }

    fn high_waters(&self) -> StageHighWaters {
        self.high_waters
    }

    fn interpretation(&self) -> GraphInterpretation<'_> {
        GraphInterpretation::new(TokenizerEpoch::of(&TokenizerConfig::text_default()), None)
            .unwrap()
    }

    fn key(
        &self,
        _: ApplicationKey<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<BaseKeyState<'_>, StageError> {
        Ok(BaseKeyState::NeverUsed)
    }

    fn entity(
        &self,
        id: EntityId,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<BaseEntity<'_>>, StageError> {
        let shape = match id {
            EntityId::Node(node) if node == self.node_a || node == self.node_b => EntityShape::Node,
            EntityId::Relationship(relationship) if relationship == self.relationship => {
                EntityShape::Relationship {
                    source: self.node_a,
                    target: self.node_b,
                    relationship_type: GraphName::new(self.relationship_type.as_str()).unwrap(),
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(BaseEntity {
            view: self.identity,
            provenance: self.provenance(id),
            shape,
            fingerprint: self.fingerprint,
            source: self,
            membership: Membership::default(),
        }))
    }

    fn has_live_incident(
        &self,
        _: NodeId,
        _: &[RelId],
        _: &mut WriteControl<'_>,
    ) -> Result<bool, StageError> {
        Ok(false)
    }

    fn property(
        &self,
        entity: EntityId,
        name: GraphName<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<PropertyValue<'_>>, StageError> {
        if entity == EntityId::Node(self.node_b) && name.as_str() == "integer" {
            return Ok(Some(
                PropertyValue::new(PropertyData::I64(99)).map_err(|_| StageError::InvalidInput)?,
            ));
        }
        Ok(None)
    }

    fn stored_text(
        &self,
        node: NodeId,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<&str>, StageError> {
        if node == self.node_b {
            return Ok(Some(self.base_text.as_str()));
        }
        Ok(None)
    }

    fn symbol(
        &self,
        _: SymbolKind,
        _: GraphName<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<Option<Symbol>, StageError> {
        Ok(None)
    }
}

struct Ze52OverlayTracer<'store> {
    store: &'store Store,
    identity: StoreInstanceId,
}

impl NativeReadConsumer<()> for Ze52OverlayTracer<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), TreeError> {
        let integer_name = String::from("integer");
        let staged_label_name = String::from("Staged");
        let label_name = String::from("Label");
        let integer = GraphName::new(integer_name.as_str()).unwrap();
        let staged_label = GraphName::new(staged_label_name.as_str()).unwrap();
        let label = GraphName::new(label_name.as_str()).unwrap();
        let node_a = NodeId::new((1_u128 << 100) + 1).unwrap();
        let node_b = NodeId::new((1_u128 << 100) + 2).unwrap();
        let relationship_one = RelId::new((1_u128 << 110) + 1).unwrap();
        let relationship_three = RelId::new((1_u128 << 110) + 3).unwrap();

        let expressions = [
            Expression::Slot(SlotId(11)),
            Expression::Slot(SlotId(12)),
            Expression::Slot(SlotId(13)),
            Expression::Slot(SlotId(14)),
            Expression::Property {
                entity: ExprId(0),
                name: integer,
            },
            Expression::Property {
                entity: ExprId(1),
                name: integer,
            },
            Expression::Unary {
                operation: UnaryExpression::StoredText,
                operand: ExprId(1),
            },
            Expression::HasLabel {
                entity: ExprId(0),
                label: staged_label,
            },
            Expression::HasLabel {
                entity: ExprId(0),
                label,
            },
            Expression::Unary {
                operation: UnaryExpression::Labels,
                operand: ExprId(0),
            },
            Expression::Unary {
                operation: UnaryExpression::Labels,
                operand: ExprId(1),
            },
            Expression::Unary {
                operation: UnaryExpression::RelType,
                operand: ExprId(2),
            },
            Expression::Unary {
                operation: UnaryExpression::RelType,
                operand: ExprId(3),
            },
        ];
        let projections: [Projection; 13] = std::array::from_fn(|index| Projection {
            slot: SlotId(100 + index as u32),
            expression: ExprId(index as u32),
        });
        let input_1 = [PlanNodeId(0)];
        let input_2 = [PlanNodeId(1)];
        let input_3 = [PlanNodeId(2)];
        let input_4 = [PlanNodeId(3)];
        let project_input = [PlanNodeId(4)];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &input_1,
                kind: OperatorKind::LookupNode {
                    output: SlotId(11),
                    id: node_a,
                },
            },
            Operator {
                inputs: &input_2,
                kind: OperatorKind::LookupNode {
                    output: SlotId(12),
                    id: node_b,
                },
            },
            Operator {
                inputs: &input_3,
                kind: OperatorKind::LookupRelationship {
                    output: SlotId(13),
                    id: relationship_one,
                },
            },
            Operator {
                inputs: &input_4,
                kind: OperatorKind::LookupRelationship {
                    output: SlotId(14),
                    id: relationship_three,
                },
            },
            Operator {
                inputs: &project_input,
                kind: OperatorKind::Project(&projections),
            },
        ];
        let mut facts = QueryArena::new(runtime.memory(), operators.len())
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        for _ in &operators {
            facts
                .push(NodeFacts::default())
                .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
                .map_err(TreeError::Runtime)?;
        }
        let mut regions = vec![
            RetainedRegion::slice(&operators).unwrap(),
            RetainedRegion::slice(&input_1).unwrap(),
            RetainedRegion::slice(&input_2).unwrap(),
            RetainedRegion::slice(&input_3).unwrap(),
            RetainedRegion::slice(&input_4).unwrap(),
            RetainedRegion::slice(&project_input).unwrap(),
            RetainedRegion::slice(&projections).unwrap(),
            RetainedRegion::slice(&expressions).unwrap(),
            RetainedRegion::declared(integer_name.as_ptr() as usize, integer_name.capacity())
                .unwrap(),
            RetainedRegion::declared(
                staged_label_name.as_ptr() as usize,
                staged_label_name.capacity(),
            )
            .unwrap(),
            RetainedRegion::declared(label_name.as_ptr() as usize, label_name.capacity()).unwrap(),
            RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                .unwrap(),
        ];
        regions.sort();
        let region_bytes = regions.capacity() * std::mem::size_of::<RetainedRegion>();
        let external_bytes = regions
            .iter()
            .map(|region| region.end() - region.start())
            .sum::<usize>()
            + region_bytes
            + std::mem::size_of::<PlanDescription<'_>>()
            + VALIDATION_SCRATCH_BYTES;
        let mut plan_capacity = runtime
            .memory()
            .reserve_external_capacity()
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        plan_capacity
            .reserve_additional(external_bytes)
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        let description = PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(5),
            eager_searches: &[],
        };
        let footprint = PlanFootprint::declared(runtime.memory().reserved_bytes());
        let (plan, facts_owner) = facts
            .validate_plan(
                description,
                footprint,
                PlanBacking::new(&regions, region_bytes).unwrap(),
                runtime.values(),
            )
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        let owners = vec![
            RetainedAllocation::array(&operators).unwrap(),
            RetainedAllocation::array(&input_1).unwrap(),
            RetainedAllocation::array(&input_2).unwrap(),
            RetainedAllocation::array(&input_3).unwrap(),
            RetainedAllocation::array(&input_4).unwrap(),
            RetainedAllocation::array(&project_input).unwrap(),
            RetainedAllocation::array(&projections).unwrap(),
            RetainedAllocation::array(&expressions).unwrap(),
            RetainedAllocation::string(&integer_name).unwrap(),
            RetainedAllocation::string(&staged_label_name).unwrap(),
            RetainedAllocation::string(&label_name).unwrap(),
            facts_owner,
        ];
        let runtime_plan = QueryInputs::reserve(
            runtime.memory(),
            RetentionInventory::vector(&owners)
                .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
                .map_err(TreeError::Runtime)?,
            runtime.values(),
        )
        .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
        .map_err(TreeError::Runtime)?
        .admit_plan(&plan, runtime.values())
        .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
        .map_err(TreeError::Runtime)?;
        let slots = [SlotId(11), SlotId(12), SlotId(13), SlotId(14)];
        let schema = Schema::new(runtime, &slots).map_err(TreeError::Runtime)?;
        let mut input = RowBatch::new(runtime, 4, 1, 128).map_err(TreeError::Runtime)?;
        input
            .push_row(
                &[
                    runtime.view().node(node_a),
                    runtime.view().node(node_b),
                    runtime.view().relationship(relationship_one),
                    runtime.view().relationship(relationship_three),
                ],
                runtime,
            )
            .map_err(TreeError::Runtime)?;
        let mut evaluator = NativeExpressionEvaluator::new(
            &runtime_plan,
            &[],
            ExpressionCapacity {
                cells: 64,
                string_bytes: 4096,
            },
            runtime,
        )
        .map_err(|_| TreeError::Invalid("ze52 expression constructor"))?;

        // The progressive overlay a mutation executor will build up clause by
        // clause. Nothing here goes through NativePattern or Mutate.
        let shared = GraphResources::from_store(self.store).unwrap();
        let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let admitted = Ze52Base::new(self.identity);
        let mut staged_labels = [staged_label];
        let mut staged_properties = [GraphProperty::new(
            integer,
            PropertyValue::new(PropertyData::I64(7)).unwrap(),
        )];
        let staged_node = CanonicalContents::node(
            &mut staged_labels,
            &mut staged_properties,
            Some("staged text"),
            None,
        )
        .unwrap();
        let mut overlay = GraphBatchReadView::new(&admitted, &writer, 8, &mut |_| Ok(())).unwrap();
        overlay
            .replace(
                BatchEntityRef::Node(NodeRef::Existing(node_a)),
                WriteImage::Node(&staged_node),
                &mut |_| Ok(()),
            )
            .unwrap();
        overlay
            .replace(
                BatchEntityRef::Relationship(RelRef::Existing(relationship_one)),
                WriteImage::Relationship {
                    source: NodeRef::Existing(node_a),
                    target: NodeRef::Existing(node_b),
                    relationship_type: GraphName::new("STAGED").unwrap(),
                    properties: &[],
                },
                &mut |_| Ok(()),
            )
            .unwrap();

        macro_rules! overlaid {
            ($expression:expr) => {{
                let mut control = |_: WritePhase| -> Result<(), StageError> { Ok(()) };
                let mut clause = ClauseOverlay::new(&mut overlay, &mut control);
                evaluator.evaluate_with_overlay(
                    $expression,
                    &schema,
                    &input,
                    0,
                    view,
                    &mut clause,
                    runtime,
                )
            }};
        }

        // 1. A pending replacement is read before the admitted base.
        assert!(matches!(
            evaluator.evaluate(ExprId(4), &schema, &input, 0, view, runtime),
            Ok(QueryValue::I64(value)) if value == i64::MIN
        ));
        assert!(matches!(overlaid!(ExprId(4)), Ok(QueryValue::I64(7))));

        // 2. Pending labels answer HasLabel and Labels.
        assert!(matches!(
            evaluator.evaluate(ExprId(8), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Bool(true))
        ));
        assert!(matches!(overlaid!(ExprId(7)), Ok(QueryValue::Bool(true))));
        assert!(matches!(overlaid!(ExprId(8)), Ok(QueryValue::Bool(false))));
        match overlaid!(ExprId(9)) {
            Ok(QueryValue::List(list)) => {
                assert_eq!(list.len(), 1);
                assert!(matches!(list.get(0), Some(QueryValue::String("Staged"))));
            }
            other => panic!("pending labels: {other:?}"),
        }

        // 3. A pending relationship type answers RelType.
        assert!(matches!(
            evaluator.evaluate(ExprId(11), &schema, &input, 0, view, runtime),
            Ok(QueryValue::String("R"))
        ));
        assert!(matches!(
            overlaid!(ExprId(11)),
            Ok(QueryValue::String("STAGED"))
        ));

        // 4. Untouched entities fall through: node b's property and text come
        // from the admitted base, its labels and its relationship's type from
        // the read view, and all four differ from the staged answers.
        assert!(matches!(
            evaluator.evaluate(ExprId(5), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Null)
        ));
        assert!(matches!(overlaid!(ExprId(5)), Ok(QueryValue::I64(99))));
        assert!(matches!(
            evaluator.evaluate(ExprId(6), &schema, &input, 0, view, runtime),
            Ok(QueryValue::String(""))
        ));
        assert!(matches!(
            overlaid!(ExprId(6)),
            Ok(QueryValue::String("admitted base text"))
        ));
        match overlaid!(ExprId(10)) {
            Ok(QueryValue::List(list)) => assert_eq!(list.len(), 0),
            other => panic!("base labels: {other:?}"),
        }
        assert!(matches!(overlaid!(ExprId(12)), Ok(QueryValue::String("R"))));

        // 5. A target this statement already deleted is typed, not silently
        // answered from the base.
        overlay
            .delete(
                BatchEntityRef::Node(NodeRef::Existing(node_b)),
                GraphDeleteMode::Detach,
                &mut |_| Ok(()),
            )
            .unwrap();
        for expression in [ExprId(5), ExprId(6), ExprId(10)] {
            let error = overlaid!(expression).unwrap_err();
            assert!(
                matches!(
                    error.failure,
                    ExpressionFailure::Stage(StageError::DeletedEntity)
                ),
                "deleted overlay target must be typed: {error}"
            );
        }

        // 6. The unchanged entry point still answers from the read view alone.
        assert!(matches!(
            evaluator.evaluate(ExprId(4), &schema, &input, 0, view, runtime),
            Ok(QueryValue::I64(value)) if value == i64::MIN
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(5), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Null)
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(8), &schema, &input, 0, view, runtime),
            Ok(QueryValue::Bool(true))
        ));
        assert!(matches!(
            evaluator.evaluate(ExprId(11), &schema, &input, 0, view, runtime),
            Ok(QueryValue::String("R"))
        ));
        match evaluator.evaluate(ExprId(9), &schema, &input, 0, view, runtime) {
            Ok(QueryValue::List(list)) => {
                assert_eq!(list.len(), 1);
                assert!(matches!(list.get(0), Some(QueryValue::String("Label"))));
            }
            other => panic!("base labels after overlay reads: {other:?}"),
        }

        assert!(overlay.counters().symbol_lookups > 0);
        assert!(overlay.counters().property_lookups > 0);
        assert!(overlay.counters().text_lookups > 0);
        Ok(())
    }
}

#[test]
fn ze52_slice_c_overlay_aware_evaluator_reads_pending_writes_then_base() {
    let directory = tempfile::tempdir().expect("store directory");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .expect("open store");
    let identity = StoreInstanceId::new(1_u128 << 92).unwrap();
    store
        .install_native_graph_for_test(expression_producer_bundle_with_extra_nodes(
            &store,
            directory.path(),
            identity,
            0,
            false,
            0,
            false,
        ))
        .unwrap();
    store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            16,
            Ze52OverlayTracer {
                store: &store,
                identity,
            },
        )
        .expect("overlay-aware expression evaluation");
    store.close().unwrap();
}
