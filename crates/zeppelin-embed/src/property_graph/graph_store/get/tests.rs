//! ZE-66 S3: typed `get_nodes`/`get_relationships` contract.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests fail loudly on the first broken contract"
)]

use super::super::{GraphStore, GraphWriteResult};
use super::{GraphGetOptions, ListKind, Value};
use crate::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, CanonicalEmbedding, EntityId, EntityKind, GraphGeneration,
    GraphName, GraphProperty, GraphRevision, NodeId, PropertyData, PropertyValue, RelId,
};

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn options() -> OpenOptions {
    OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024)
}

fn revision(value: u64) -> GraphRevision {
    GraphRevision::new(value).expect("positive revision")
}

fn generation(value: u64) -> GraphGeneration {
    GraphGeneration::new(value)
}

fn node_key(key: &str) -> ApplicationKey<'_> {
    ApplicationKey::new(EntityKind::Node, "app", key).expect("node key")
}

fn node_id(result: &GraphWriteResult, index: usize) -> NodeId {
    match result.receipts()[index].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("receipt {index} names a relationship"),
    }
}

fn rel_id(result: &GraphWriteResult, index: usize) -> RelId {
    match result.receipts()[index].entity {
        EntityId::Relationship(relationship) => relationship,
        EntityId::Node(_) => panic!("receipt {index} names a node"),
    }
}

fn tower() -> EmbeddingTower {
    EmbeddingTower {
        model_id: "doc".into(),
        model_version: "1".into(),
        weights_digest: vec![0x42],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 3,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    }
}

#[test]
fn graph_store_get_nodes_distinguishes_absent_from_present_empty_text() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        GraphStore::create(parent.path().join("text"), options(), None).expect("graph store");
    let no_text = CanonicalContents::node(&mut [], &mut [], None, None).expect("no-text image");
    let empty_text =
        CanonicalContents::node(&mut [], &mut [], Some(""), None).expect("empty-text image");
    let created = store
        .apply_batch(
            &[
                StructuredWrite {
                    key: node_key("absent"),
                    revision: revision(1),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&no_text)),
                },
                StructuredWrite {
                    key: node_key("empty"),
                    revision: revision(1),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&empty_text)),
                },
            ],
            &control(),
        )
        .expect("create both nodes");
    let absent_id = node_id(&created, 0);
    let empty_id = node_id(&created, 1);

    let result = store
        .get_nodes(
            &[absent_id, empty_id],
            GraphGetOptions {
                text: true,
                vector: false,
            },
            &control(),
        )
        .expect("get nodes");
    let nodes = result.nodes();
    let absent_node = nodes[0].as_ref().expect("absent-text node present");
    assert_eq!(absent_node.text, None, "no stored text must stay None");
    let empty_node = nodes[1].as_ref().expect("empty-text node present");
    let text_span = empty_node.text.expect("present-empty text must be Some");
    assert_eq!(result.string(text_span), Some(""));

    // Without the explicit selection, text is never copied even though it
    // is stored: field selection is opt-in.
    let unselected = store
        .get_nodes(&[empty_id], GraphGetOptions::default(), &control())
        .expect("get nodes without text selection");
    assert_eq!(
        unselected.nodes()[0].as_ref().expect("node present").text,
        None
    );
    store.close().expect("close store");
}

#[test]
fn ze211_nodes_bulk_pools_match_accessors() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store = GraphStore::create(parent.path().join("lists"), options(), Some(tower()))
        .expect("graph store");
    let sentinel =
        PropertyValue::new(PropertyData::EmptyList { count: 0 }).expect("untyped empty list value");
    let typed = PropertyValue::new(PropertyData::Strings(&[])).expect("typed empty list value");
    let mut properties = [
        GraphProperty::new(
            GraphName::new("children").unwrap(),
            PropertyValue::new(PropertyData::Strings(&["a\0λ"])).unwrap(),
        ),
        GraphProperty::new(GraphName::new("sentinel").expect("name"), sentinel),
        GraphProperty::new(GraphName::new("typed").expect("name"), typed),
    ];
    let document = tower();
    let embedding = CanonicalEmbedding::new(&document, &[1.5, -2.5]).unwrap();
    let mut labels = [GraphName::new("Label").unwrap()];
    let image =
        CanonicalContents::node(&mut labels, &mut properties, Some("text"), Some(embedding))
            .expect("list image");
    let created = store
        .apply_batch(
            &[StructuredWrite {
                key: node_key("lists"),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect("create node with list properties");
    let id = node_id(&created, 0);

    let result = store
        .get_nodes(
            &[id],
            GraphGetOptions {
                text: true,
                vector: true,
            },
            &control(),
        )
        .expect("get node with list properties");
    let node = result.nodes()[0].as_ref().expect("node present");
    let properties = result.properties(node.properties);
    // Exactly three properties: "absent" was never written, so it is not a
    // fourth row with some null-like marker -- it simply does not appear.
    assert_eq!(
        properties.len(),
        3,
        "absent property must not appear at all"
    );

    let sentinel_property = properties
        .iter()
        .find(|property| result.string(property.name) == Some("sentinel"))
        .expect("sentinel property present");
    let sentinel_value = result
        .value(sentinel_property.value)
        .expect("sentinel value");
    assert!(
        matches!(
            sentinel_value,
            Value::List {
                element: ListKind::Empty,
                ..
            }
        ),
        "expected the untyped EmptyList sentinel, got {sentinel_value:?}"
    );

    let typed_property = properties
        .iter()
        .find(|property| result.string(property.name) == Some("typed"))
        .expect("typed property present");
    let typed_value = result.value(typed_property.value).expect("typed value");
    assert!(
        matches!(
            typed_value,
            Value::List {
                element: ListKind::String,
                ..
            }
        ),
        "expected a typed empty string list, got {typed_value:?}"
    );
    assert_ne!(
        sentinel_value, typed_value,
        "the untyped sentinel and a typed empty list must remain distinct"
    );
    store.close().expect("close store");
    let pools = result.pools();
    for (index, value) in pools.values.iter().enumerate() {
        assert_eq!(result.value(super::ValueIndex(index as u32)), Some(value));
    }
    for property in pools.properties {
        assert_eq!(
            result.string(property.name).unwrap().as_bytes(),
            &pools.bytes[super::range(property.name)]
        );
    }
    assert_eq!(pools.properties, result.properties(node.properties));
    assert_eq!(result.labels(node), &pools.names[super::range(node.labels)]);
    assert_eq!(
        result.vector(node.vector.unwrap()),
        &pools.vectors[super::range(node.vector.unwrap())]
    );
    assert_eq!(pools.vectors, &[1.5_f32.to_bits(), (-2.5_f32).to_bits()]);
    assert_eq!(
        result.string(node.text.unwrap()).unwrap().as_bytes(),
        &pools.bytes[super::range(node.text.unwrap())]
    );
    assert!(!pools.children.is_empty());
    for value in pools.values {
        if let Value::List { children, .. } = value {
            assert_eq!(
                result.children(*children),
                &pools.children[super::range(*children)]
            );
        }
    }
    assert!(pools.nodes.is_empty() && pools.relationships.is_empty());
}

#[test]
fn graph_store_get_nodes_round_trips_full_128_bit_ids() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("wide");
    let first_node = NodeId::new((1_u128 << 64) + 11).expect("wide node seed");
    let first_relationship = RelId::new((1_u128 << 100) + 13).expect("wide relationship seed");
    let store = GraphStore::create_with_allocator_seed_for_test(
        &path,
        options(),
        first_node,
        first_relationship,
    )
    .expect("seeded graph store");
    let image = CanonicalContents::node(&mut [], &mut [], Some("wide"), None).expect("image");
    let created = store
        .apply_batch(
            &[StructuredWrite {
                key: node_key("wide"),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect("create wide node");
    let id = node_id(&created, 0);
    assert!(id.get() > u128::from(u64::MAX));
    assert_eq!(id, first_node);

    let result = store
        .get_nodes(
            &[id],
            GraphGetOptions {
                text: true,
                vector: false,
            },
            &control(),
        )
        .expect("get wide node");
    let node = result.nodes()[0].as_ref().expect("wide node present");
    assert_eq!(node.id, first_node);
    assert!(node.id.get() > u128::from(u64::MAX));
    let text_span = node.text.expect("stored text present");
    assert_eq!(result.string(text_span), Some("wide"));
    store.close().expect("close store");
}

#[test]
fn graph_store_get_nodes_returns_none_for_a_missing_id() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        GraphStore::create(parent.path().join("missing"), options(), None).expect("graph store");
    let never_written = NodeId::new(999_999).expect("unused node id");
    let result = store
        .get_nodes(&[never_written], GraphGetOptions::default(), &control())
        .expect("get missing node");
    assert_eq!(result.nodes(), &[None]);
    store.close().expect("close store");
}

#[test]
fn graph_store_get_nodes_result_stays_readable_after_close() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        GraphStore::create(parent.path().join("closed"), options(), None).expect("graph store");
    let image = CanonicalContents::node(&mut [], &mut [], Some("kept"), None).expect("image");
    let created = store
        .apply_batch(
            &[StructuredWrite {
                key: node_key("kept"),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect("create node");
    let id = node_id(&created, 0);
    let result = store
        .get_nodes(
            &[id],
            GraphGetOptions {
                text: true,
                vector: false,
            },
            &control(),
        )
        .expect("get node");
    store.close().expect("close store");
    drop(store);

    let node = result.nodes()[0]
        .as_ref()
        .expect("node present after close");
    assert_eq!(node.id, id);
    let text_span = node.text.expect("text present after close");
    assert_eq!(result.string(text_span), Some("kept"));
}

#[test]
fn graph_store_get_nodes_admits_one_generation_for_the_whole_call() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        GraphStore::create(parent.path().join("generation"), options(), None).expect("graph store");
    let image = CanonicalContents::node(&mut [], &mut [], Some("x"), None).expect("image");
    let first = store
        .apply_batch(
            &[StructuredWrite {
                key: node_key("first"),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect("create first node");
    let first_id = node_id(&first, 0);
    let second = store
        .apply_batch(
            &[StructuredWrite {
                key: node_key("second"),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect("create second node");
    let second_id = node_id(&second, 0);
    assert_eq!(
        first.outcome(),
        crate::property_graph::GraphWriteOutcome::Committed {
            generation: generation(2)
        }
    );
    assert_eq!(
        second.outcome(),
        crate::property_graph::GraphWriteOutcome::Committed {
            generation: generation(3)
        }
    );

    // One call, two entities installed at two different generations: this is
    // one atomic read admission, not two independent point reads.
    let result = store
        .get_nodes(
            &[first_id, second_id],
            GraphGetOptions::default(),
            &control(),
        )
        .expect("get both nodes in one call");
    assert_eq!(
        result.generation(),
        generation(3),
        "the whole call observes one current admitted generation"
    );
    let first_node = result.nodes()[0].as_ref().expect("first node present");
    let second_node = result.nodes()[1].as_ref().expect("second node present");
    assert_eq!(
        first_node.generation,
        generation(2),
        "each entity keeps its own installing generation"
    );
    assert_eq!(second_node.generation, generation(3));
    store.close().expect("close store");
}

#[test]
fn graph_store_get_nodes_selects_the_stored_vector_only_when_requested() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store = GraphStore::create(parent.path().join("vector"), options(), Some(tower()))
        .expect("graph store with a document tower");
    let document = tower();
    let embedding = CanonicalEmbedding::new(&document, &[1.5, -2.5]).expect("canonical embedding");
    let image = CanonicalContents::node(&mut [], &mut [], None, Some(embedding))
        .expect("node with a vector");
    let created = store
        .apply_batch(
            &[StructuredWrite {
                key: node_key("vector"),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect("create vectored node");
    let id = node_id(&created, 0);

    let unselected = store
        .get_nodes(&[id], GraphGetOptions::default(), &control())
        .expect("get without vector selection");
    assert_eq!(
        unselected.nodes()[0].as_ref().expect("node present").vector,
        None
    );

    let selected = store
        .get_nodes(
            &[id],
            GraphGetOptions {
                text: false,
                vector: true,
            },
            &control(),
        )
        .expect("get with vector selection");
    let node = selected.nodes()[0].as_ref().expect("node present");
    let span = node.vector.expect("vector present when selected");
    let coordinates: Vec<f32> = selected
        .vector(span)
        .iter()
        .map(|bits| f32::from_bits(*bits))
        .collect();
    assert_eq!(coordinates, vec![1.5, -2.5]);
    store.close().expect("close store");
}

#[test]
fn ze211_relationships_bulk_pools_match_accessors() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        GraphStore::create(parent.path().join("edges"), options(), None).expect("graph store");
    let source_image = CanonicalContents::node(&mut [], &mut [], None, None).expect("source");
    let target_image = CanonicalContents::node(&mut [], &mut [], None, None).expect("target");
    let created_nodes = store
        .apply_batch(
            &[
                StructuredWrite {
                    key: node_key("source"),
                    revision: revision(1),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&source_image)),
                },
                StructuredWrite {
                    key: node_key("target"),
                    revision: revision(1),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&target_image)),
                },
            ],
            &control(),
        )
        .expect("create endpoints");
    let source_id = node_id(&created_nodes, 0);
    let target_id = node_id(&created_nodes, 1);

    let weight = PropertyValue::new(PropertyData::I64(42)).expect("weight value");
    let created_edge = store
        .apply_batch(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "app", "edge")
                    .expect("relationship key"),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: crate::property_graph::NodeRef::Existing(source_id),
                    target: crate::property_graph::NodeRef::Existing(target_id),
                    relationship_type: GraphName::new("LINKS").expect("type"),
                    properties: &[GraphProperty::new(
                        GraphName::new("weight").expect("name"),
                        weight,
                    )],
                }),
            }],
            &control(),
        )
        .expect("create relationship");
    let edge_id = rel_id(&created_edge, 0);

    let result = store
        .get_relationships(&[edge_id], &control())
        .expect("get relationship");
    let relationship = result.relationships()[0]
        .as_ref()
        .expect("relationship present");
    assert_eq!(relationship.id, edge_id);
    assert_eq!(relationship.source, source_id);
    assert_eq!(relationship.target, target_id);
    assert_eq!(result.string(relationship.relationship_type), Some("LINKS"));
    let properties = result.properties(relationship.properties);
    assert_eq!(properties.len(), 1);
    assert_eq!(result.string(properties[0].name), Some("weight"));
    let value = result.value(properties[0].value).expect("weight value");
    assert_eq!(*value, Value::I64(42));
    store.close().expect("close store");
    let pools = result.pools();
    for (index, value) in pools.values.iter().enumerate() {
        assert_eq!(result.value(super::ValueIndex(index as u32)), Some(value));
    }
    for property in pools.properties {
        assert_eq!(
            result.string(property.name).unwrap().as_bytes(),
            &pools.bytes[super::range(property.name)]
        );
    }
    assert_eq!(pools.properties, result.properties(relationship.properties));
    assert!(pools.nodes.is_empty() && pools.relationships.is_empty());
}

#[test]
fn graph_store_get_relationships_returns_none_for_a_missing_id() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store = GraphStore::create(parent.path().join("missing-edge"), options(), None)
        .expect("graph store");
    let never_written = RelId::new(999_999).expect("unused relationship id");
    let result = store
        .get_relationships(&[never_written], &control())
        .expect("get missing relationship");
    assert_eq!(result.relationships(), &[None]);
    store.close().expect("close store");
}

#[test]
fn graph_store_get_nodes_copies_the_application_key() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        GraphStore::create(parent.path().join("keyed"), options(), None).expect("graph store");
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("image");
    let created = store
        .apply_batch(
            &[StructuredWrite {
                key: node_key("keyed"),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect("create keyed node");
    let id = node_id(&created, 0);

    let result = store
        .get_nodes(&[id], GraphGetOptions::default(), &control())
        .expect("get keyed node");
    let node = result.nodes()[0].as_ref().expect("node present");
    let key = node.key.expect("application key present");
    assert_eq!(key.kind, EntityKind::Node);
    assert_eq!(result.string(key.namespace), Some("app"));
    assert_eq!(result.string(key.value), Some("keyed"));
    store.close().expect("close store");
}

#[test]
fn graph_store_get_nodes_orders_multiple_labels_by_name() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        GraphStore::create(parent.path().join("labels"), options(), None).expect("graph store");
    let mut labels = [
        GraphName::new("Zebra").expect("label"),
        GraphName::new("Apple").expect("label"),
        GraphName::new("Mango").expect("label"),
    ];
    let image = CanonicalContents::node(&mut labels, &mut [], None, None).expect("image");
    let created = store
        .apply_batch(
            &[StructuredWrite {
                key: node_key("labeled"),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect("create labeled node");
    let id = node_id(&created, 0);

    let result = store
        .get_nodes(&[id], GraphGetOptions::default(), &control())
        .expect("get labeled node");
    let node = result.nodes()[0].as_ref().expect("node present");
    let label_names: Vec<&str> = result
        .labels(node)
        .iter()
        .map(|span| result.string(*span).expect("label text"))
        .collect();
    assert_eq!(label_names, vec!["Apple", "Mango", "Zebra"]);
    store.close().expect("close store");
}

#[test]
fn get_nodes_over_max_graph_changes_is_refused_as_limit_before_any_read() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        GraphStore::create(parent.path().join("nodes-limit"), options(), None).expect("store");
    let too_many: Vec<NodeId> = (1..=(crate::property_graph::MAX_GRAPH_CHANGES as u128 + 1))
        .map(|value| NodeId::new(value).expect("nonzero id"))
        .collect();

    let Err(error) = store.get_nodes(&too_many, GraphGetOptions::default(), &control()) else {
        panic!("oversized batch must be refused");
    };
    assert_eq!(
        error.kind(),
        crate::property_graph::GraphStoreErrorKind::Limit
    );
    store.close().expect("close store");
}

#[test]
fn get_relationships_over_max_graph_changes_is_refused_as_limit_before_any_read() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        GraphStore::create(parent.path().join("rels-limit"), options(), None).expect("store");
    let too_many: Vec<RelId> = (1..=(crate::property_graph::MAX_GRAPH_CHANGES as u128 + 1))
        .map(|value| RelId::new(value).expect("nonzero id"))
        .collect();

    let Err(error) = store.get_relationships(&too_many, &control()) else {
        panic!("oversized batch must be refused");
    };
    assert_eq!(
        error.kind(),
        crate::property_graph::GraphStoreErrorKind::Limit
    );
    store.close().expect("close store");
}
