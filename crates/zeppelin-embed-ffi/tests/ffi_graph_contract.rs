//! Graph data-only contract tests; these do not exercise graph store admission.
#![cfg(feature = "graph-cypher")]
use zeppelin_embed::property_graph::{NodeId, RelId};
use zeppelin_embed_ffi::*;

#[test]
fn graph_identity_conversion_retains_high_words_and_rejects_zero() {
    for raw in [1, (1u128 << 64) + 1, (1u128 << 127) + 7, u128::MAX] {
        let node = NodeId::new(raw).unwrap();
        let rel = RelId::new(raw).unwrap();
        let c_node = ZeNodeId::from(node);
        let c_rel = ZeRelId::from(rel);
        assert_eq!(c_node.high, (raw >> 64) as u64);
        assert_eq!(c_node.low, raw as u64);
        assert_eq!(NodeId::try_from(c_node).unwrap(), node);
        assert_eq!(RelId::try_from(c_rel).unwrap(), rel);
    }
    assert_eq!(
        NodeId::try_from(ZeNodeId { high: 0, low: 0 }),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
    assert_eq!(
        RelId::try_from(ZeRelId { high: 0, low: 0 }),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
}

#[test]
fn graph_empty_list_shape_rejects_nonzero_count_and_preserves_typed_empties() {
    let mut value = ZeGraphValue {
        abi_size: std::mem::size_of::<ZeGraphValue>() as u32,
        abi_reserved: 0,
        tag: ZeGraphValueTag::ZeGraphValueList as u32,
        list_kind: ZeGraphListKind::ZeGraphListEmpty as u32,
        boolean: 0,
        entity_index: 0,
        integer: 0,
        floating: 0.0,
        range: ZeGraphRange { start: 0, count: 0 },
    };
    assert_eq!(value.validate_shape(), Ok(()));
    assert_eq!(value.validate_stored_shape(), Ok(()));
    value.range.count = 1;
    assert_eq!(
        value.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
    value.range.count = 0;
    for kind in [
        ZeGraphListKind::ZeGraphListBool,
        ZeGraphListKind::ZeGraphListI64,
        ZeGraphListKind::ZeGraphListF64,
        ZeGraphListKind::ZeGraphListString,
    ] {
        value.list_kind = kind as u32;
        assert_eq!(value.validate_stored_shape(), Ok(()));
    }
    value.list_kind = ZeGraphListKind::ZeGraphListQuery as u32;
    assert_eq!(value.validate_shape(), Ok(()));
    assert_eq!(
        value.validate_stored_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
}

#[test]
fn graph_native_error_mapping_preserves_type_arithmetic_and_retry_conflicts() {
    use zeppelin_embed::property_graph::query::{QueryError as Q, plan::PlanError as P};
    use zeppelin_embed::property_graph::{GraphRevision, KeyLifecycleError as K};
    for (error, code) in [
        (Q::Type, ZeErrorCode::ZeErrType),
        (Q::ArithmeticDomain, ZeErrorCode::ZeErrArithmeticDomain),
        (Q::ArithmeticOverflow, ZeErrorCode::ZeErrArithmeticOverflow),
        (Q::DivisionByZero, ZeErrorCode::ZeErrDivisionByZero),
        (Q::Cancelled, ZeErrorCode::ZeErrCancelled),
        (Q::ReadCancelled, ZeErrorCode::ZeErrCancelled),
        (Q::Timeout, ZeErrorCode::ZeErrTimeout),
        (Q::WorkLimit, ZeErrorCode::ZeErrBudgetExceeded),
    ] {
        assert_eq!(ZeErrorCode::from(error), code);
    }
    for (error, code) in [
        (P::Scope, ZeErrorCode::ZeErrScope),
        (P::Parameter, ZeErrorCode::ZeErrParameter),
        (P::Type, ZeErrorCode::ZeErrType),
        (P::Control(Q::Cancelled), ZeErrorCode::ZeErrCancelled),
    ] {
        assert_eq!(ZeErrorCode::from(error), code);
    }
    for (error, code) in [
        (K::AlreadyExists, ZeErrorCode::ZeErrKeyConflict),
        (
            K::IncarnationConflict,
            ZeErrorCode::ZeErrIncarnationConflict,
        ),
        (
            K::DeletionRevisionConflict,
            ZeErrorCode::ZeErrDeletionRevisionConflict,
        ),
        (
            K::Stale {
                current: GraphRevision::new(7).unwrap(),
            },
            ZeErrorCode::ZeErrStaleRevision,
        ),
        (K::GenerationOverflow, ZeErrorCode::ZeErrGenerationOverflow),
        (K::RevisionOverflow, ZeErrorCode::ZeErrRevisionOverflow),
        (K::DuplicateTarget, ZeErrorCode::ZeErrDuplicateTarget),
    ] {
        assert_eq!(ZeErrorCode::from(error), code);
    }
}

#[test]
fn graph_endpoint_shape_keeps_local_and_full_identity_preconditions_separate() {
    let mut endpoint = ZeGraphEndpoint {
        abi_size: std::mem::size_of::<ZeGraphEndpoint>() as u32,
        abi_reserved: 0,
        kind: ZeGraphEndpointKind::ZeGraphEndpointNode as u32,
        local_item: 0,
        node: ZeNodeId { high: 1, low: 0 },
    };
    assert_eq!(endpoint.validate_shape(), Ok(()));
    endpoint.local_item = 1;
    assert_eq!(
        endpoint.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
    endpoint.local_item = 0;
    endpoint.node.high = 0;
    assert_eq!(
        endpoint.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
    endpoint.kind = ZeGraphEndpointKind::ZeGraphEndpointLocal as u32;
    assert_eq!(endpoint.validate_shape(), Ok(()));
    endpoint.local_item = 16_383;
    assert_eq!(endpoint.validate_shape(), Ok(()));
    endpoint.local_item = 16_384;
    assert_eq!(
        endpoint.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
    endpoint.local_item = 0;
    endpoint.kind = 3;
    assert_eq!(
        endpoint.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
    endpoint.kind = 0;
    assert_eq!(endpoint.validate_shape(), Ok(()));
    endpoint.abi_size += 8;
    assert_eq!(
        endpoint.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
}

#[test]
fn graph_batch_shape_preserves_empty_and_nul_names_with_explicit_image_presence() {
    use zeppelin_embed::property_graph::{ApplicationKey, EntityKind, GraphName};
    let zero_endpoint = ZeGraphEndpoint {
        abi_size: std::mem::size_of::<ZeGraphEndpoint>() as u32,
        abi_reserved: 0,
        kind: 0,
        local_item: 0,
        node: ZeNodeId::default(),
    };
    let mut item = ZeGraphBatchItem {
        abi_size: std::mem::size_of::<ZeGraphBatchItem>() as u32,
        abi_reserved: 0,
        entity_kind: 0,
        operation: 0,
        namespace_name: ZeGraphRange::default(),
        key: ZeGraphRange::default(),
        revision: 1,
        expected_node: ZeNodeId::default(),
        expected_relationship: ZeRelId::default(),
        expected_deletion_revision: 0,
        delete_mode: 0,
        has_image: 1,
        image: 0,
        reserved: 0,
        source: zero_endpoint,
        target: zero_endpoint,
    };
    assert_eq!(item.validate_shape(), Ok(()));
    for (namespace, key) in [("", ""), ("a\0b", "\0")] {
        assert!(ApplicationKey::new(EntityKind::Node, namespace, key).is_ok());
        assert!(GraphName::new(namespace).is_ok());
        item.namespace_name.count = namespace.len() as u32;
        item.key.count = key.len() as u32;
        assert_eq!(item.validate_shape(), Ok(()));
    }
    item.has_image = 0;
    assert_eq!(
        item.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
    item.has_image = 1;
    item.expected_node.high = 1;
    assert_eq!(
        item.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
    item.operation = ZeGraphBatchOperation::ZeGraphBatchPut as u32;
    assert_eq!(item.validate_shape(), Ok(()));
    item.operation = ZeGraphBatchOperation::ZeGraphBatchDelete as u32;
    item.has_image = 0;
    item.delete_mode = 1;
    assert_eq!(item.validate_shape(), Ok(()));
    item.operation = ZeGraphBatchOperation::ZeGraphBatchRecreate as u32;
    item.has_image = 1;
    item.delete_mode = 0;
    item.expected_node.high = 0;
    item.expected_deletion_revision = 1;
    item.revision = 2;
    assert_eq!(item.validate_shape(), Ok(()));
    item.expected_deletion_revision = 0;
    assert_eq!(
        item.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
}

#[test]
fn graph_tightening_distinguishes_explicit_zero_from_absent_and_rejects_widening() {
    let mut limit = ZeGraphWorkLimit {
        abi_size: std::mem::size_of::<ZeGraphWorkLimit>() as u32,
        abi_reserved: 0,
        kind: ZeGraphWorkKind::ZeGraphWorkExpressions as u32,
        reserved: 0,
        limit: 0,
    };
    assert_eq!(limit.validate_shape(), Ok(()));
    limit.limit = 8_000_000;
    assert_eq!(limit.validate_shape(), Ok(()));
    limit.limit += 1;
    assert_eq!(
        limit.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
    limit.limit = 0;
    limit.kind = ZeGraphWorkKind::ZeGraphWorkPeakOwnedBytes as u32;
    assert_eq!(
        limit.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
    let mut limits = ZeGraphQueryLimits {
        abi_size: std::mem::size_of::<ZeGraphQueryLimits>() as u32,
        abi_reserved: 0,
        has_query_bytes: 0,
        reserved: 0,
        query_bytes: 0,
        work: std::ptr::null(),
        work_count: 0,
    };
    assert_eq!(limits.validate_shape(), Ok(()));
    limits.has_query_bytes = 1;
    assert_eq!(limits.validate_shape(), Ok(()));
    limits.query_bytes = 24 * 1024 * 1024;
    assert_eq!(limits.validate_shape(), Ok(()));
    limits.query_bytes += 1;
    assert_eq!(
        limits.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
    limits.query_bytes = 7;
    limits.has_query_bytes = 0;
    assert_eq!(
        limits.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
}

#[test]
fn graph_nested_array_headers_require_exact_v1_stride_and_zero_reserved() {
    let mut property = ZeGraphProperty {
        abi_size: 24,
        abi_reserved: 0,
        name: ZeGraphRange::default(),
        value: 0,
        reserved: 0,
    };
    assert_eq!(property.validate_header(), Ok(()));
    for size in [0, 8, 23, 25, 65536, u32::MAX] {
        property.abi_size = size;
        assert_eq!(
            property.validate_header(),
            Err(ZeErrorCode::ZeErrInvalidArgument)
        );
    }
    property.abi_size = 24;
    property.abi_reserved = 1;
    assert_eq!(
        property.validate_header(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
}

#[test]
fn graph_search_shape_preserves_tier_presence_and_rejects_wrong_kind_options() {
    // All fields are integer/pointer/floating representations; zero is valid storage.
    let mut search: ZeGraphSearch = unsafe { std::mem::zeroed() };
    search.abi_size = std::mem::size_of::<ZeGraphSearch>() as u32;
    search.vector = ZeGraphOptionalIndex {
        present: 1,
        index: 0,
    };
    search.node_slot = 1;
    search.score_slot = 2;
    assert_eq!(search.validate_shape(), Ok(()));
    search.has_tier = 1;
    for tier in 0..=3 {
        search.tier = tier;
        assert_eq!(search.validate_shape(), Ok(()));
    }
    search.has_tier = 0;
    assert_eq!(
        search.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
    search.tier = 0;
    search.kind = ZeGraphSearchKind::ZeGraphSearchHybrid as u32;
    assert_eq!(
        search.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
    search.text = ZeGraphOptionalIndex {
        present: 1,
        index: 0,
    };
    search.vector_distance_slot = ZeGraphOptionalIndex {
        present: 1,
        index: 9,
    };
    search.lexical_score_slot = ZeGraphOptionalIndex {
        present: 1,
        index: 10,
    };
    search.eligible_set = ZeGraphOptionalIndex {
        present: 1,
        index: u32::MAX,
    };
    assert_eq!(search.validate_shape(), Ok(()));
    search.kind = ZeGraphSearchKind::ZeGraphSearchText as u32;
    assert_eq!(
        search.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
    search.kind = 3;
    assert_eq!(
        search.validate_shape(),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
}

#[test]
fn graph_compile_limits_preserve_zero_and_reject_each_widened_guard() {
    let limits = ZeGraphCompileLimits {
        abi_size: std::mem::size_of::<ZeGraphCompileLimits>() as u32,
        abi_reserved: 0,
        text_bytes: 65536,
        tokens: 8192,
        ast_nodes: 4096,
        depth: 64,
        parameters: 256,
        columns: 256,
        list_depth: 16,
        path_hops: 16,
    };
    assert_eq!(limits.validate_shape(), Ok(()));
    for field in 0..8 {
        let mut invalid = limits;
        let mut zero = limits;
        fn select(value: &mut ZeGraphCompileLimits, field: u32) -> &mut u32 {
            match field {
                0 => &mut value.text_bytes,
                1 => &mut value.tokens,
                2 => &mut value.ast_nodes,
                3 => &mut value.depth,
                4 => &mut value.parameters,
                5 => &mut value.columns,
                6 => &mut value.list_depth,
                _ => &mut value.path_hops,
            }
        }
        *select(&mut invalid, field) += 1;
        *select(&mut zero, field) = 0;
        assert_eq!(
            invalid.validate_shape(),
            Err(ZeErrorCode::ZeErrInvalidArgument)
        );
        assert_eq!(zero.validate_shape(), Ok(()));
    }
}

#[test]
fn graph_value_shapes_preserve_ieee_bits_and_reject_inactive_or_overflowing_fields() {
    let base = ZeGraphValue {
        abi_size: 48,
        abi_reserved: 0,
        tag: 0,
        list_kind: 0,
        boolean: 0,
        entity_index: 0,
        integer: 0,
        floating: 0.0,
        range: ZeGraphRange::default(),
    };
    for bits in [
        0,
        1u64 << 63,
        0x7ff0000000000000,
        0xfff0000000000000,
        0x7ff8000000000001,
        0x7ff0000000000001,
        u64::MAX,
    ] {
        let value = ZeGraphValue {
            tag: 3,
            floating: f64::from_bits(bits),
            ..base
        };
        assert_eq!(value.validate_shape(), Ok(()));
        assert_eq!(value.floating.to_bits(), bits);
        assert_eq!(value.validate_stored_shape(), Ok(()));
    }
    for invalid in [
        ZeGraphValue { tag: 8, ..base },
        ZeGraphValue {
            abi_reserved: 1,
            ..base
        },
        ZeGraphValue {
            tag: 1,
            boolean: 2,
            ..base
        },
        ZeGraphValue {
            list_kind: 1,
            ..base
        },
        ZeGraphValue {
            entity_index: 1,
            ..base
        },
        ZeGraphValue { integer: 1, ..base },
        ZeGraphValue {
            floating: -0.0,
            ..base
        },
        ZeGraphValue {
            range: ZeGraphRange { start: 0, count: 1 },
            ..base
        },
        ZeGraphValue {
            tag: 7,
            list_kind: 6,
            ..base
        },
        ZeGraphValue {
            tag: 7,
            range: ZeGraphRange {
                start: 0,
                count: 524289,
            },
            ..base
        },
        ZeGraphValue {
            tag: 4,
            range: ZeGraphRange {
                start: u32::MAX,
                count: 1,
            },
            ..base
        },
    ] {
        assert_eq!(
            invalid.validate_shape(),
            Err(ZeErrorCode::ZeErrInvalidArgument)
        );
    }
    for tag in [0, 5, 6] {
        assert_eq!(
            ZeGraphValue { tag, ..base }.validate_stored_shape(),
            Err(ZeErrorCode::ZeErrInvalidArgument)
        );
    }
    assert_eq!(
        ZeGraphRange { start: 3, count: 2 }.checked_range(5),
        Ok(3..5)
    );
    assert_eq!(
        ZeGraphRange { start: 3, count: 3 }.checked_range(5),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
    assert_eq!(
        ZeGraphRange {
            start: u32::MAX,
            count: 0
        }
        .checked_range(usize::MAX),
        Ok(u32::MAX as usize..u32::MAX as usize)
    );
    assert_eq!(
        ZeGraphRange {
            start: u32::MAX,
            count: 1
        }
        .checked_range(usize::MAX),
        Err(ZeErrorCode::ZeErrInvalidArgument)
    );
}
