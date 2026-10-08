//! Frozen ABI v1 layouts measured independently by clang on macOS arm64.
#![cfg(feature = "graph-cypher")]
use std::mem::{align_of, offset_of, size_of};
use zeppelin_embed_ffi::*;

macro_rules! layout {
    ($ty:ty, $size:expr, $align:expr; $( $field:ident => $offset:expr ),* $(,)?) => {
        assert_eq!(size_of::<$ty>(), $size, "size of {}", stringify!($ty));
        assert_eq!(align_of::<$ty>(), $align, "alignment of {}", stringify!($ty));
        $(assert_eq!(offset_of!($ty, $field), $offset, "{}.{}", stringify!($ty), stringify!($field));)*
    };
}

#[test]
fn every_graph_struct_has_the_frozen_c_size_alignment_and_field_offsets() {
    layout!(ZeNodeId, 16, 8; high => 0, low => 8);
    layout!(ZeRelId, 16, 8; high => 0, low => 8);
    layout!(ZeGraphResources, 40, 8; abi_size => 0, abi_reserved => 4, engine_bytes => 8, engine_peak_bytes => 16, application_bytes => 24, application_peak_bytes => 32);
    layout!(ZeGraphRange, 8, 4; start => 0, count => 4);
    layout!(ZeGraphValue, 48, 8; abi_size => 0, abi_reserved => 4, tag => 8, list_kind => 12, boolean => 16, entity_index => 20, integer => 24, floating => 32, range => 40);
    layout!(ZeGraphBytes, 16, 8; data => 0, count => 8);
    layout!(ZeGraphControl, 24, 8; abi_size => 0, abi_reserved => 4, cancel_token => 8, deadline_ns => 16);
    layout!(ZeGraphProperty, 24, 4; abi_size => 0, abi_reserved => 4, name => 8, value => 16, reserved => 20);
    layout!(ZeGraphNode, 104, 8; abi_size => 0, abi_reserved => 4, id => 8, has_key => 24, has_text => 28, has_vector => 32, reserved => 36, namespace_name => 40, key => 48, revision => 56, last_change_generation => 64, properties => 72, text => 80, vector => 88, labels => 96);
    layout!(ZeGraphRelationship, 112, 8; abi_size => 0, abi_reserved => 4, id => 8, source => 24, target => 40, has_key => 56, reserved => 60, namespace_name => 64, key => 72, revision => 80, last_change_generation => 88, properties => 96, relationship_type => 104);
    layout!(ZeGraphValuePool, 136, 8; abi_size => 0, abi_reserved => 4, values => 8, value_count => 16, children => 24, child_count => 32, bytes => 40, byte_count => 48, nodes => 56, node_count => 64, relationships => 72, relationship_count => 80, properties => 88, property_count => 96, names => 104, name_count => 112, vectors => 120, vector_count => 128);
    layout!(ZeGraphEndpoint, 32, 8; abi_size => 0, abi_reserved => 4, kind => 8, local_item => 12, node => 16);
    layout!(ZeGraphBatchItem, 160, 8; abi_size => 0, abi_reserved => 4, entity_kind => 8, operation => 12, namespace_name => 16, key => 24, revision => 32, expected_node => 40, expected_relationship => 56, expected_deletion_revision => 72, delete_mode => 80, has_image => 84, image => 88, reserved => 92, source => 96, target => 128);
    layout!(ZeGraphBatchRequest, 40, 8; abi_size => 0, abi_reserved => 4, items => 8, item_count => 16, pool => 24, control => 32);
    layout!(ZeGraphOptionalIndex, 8, 4; present => 0, index => 4);
    layout!(ZeGraphExpression, 56, 4; abi_size => 0, abi_reserved => 4, kind => 8, operation => 12, left => 16, right => 20, has_operand => 24, distinct => 28, value => 32, reserved => 36, name => 40, children => 48);
    layout!(ZeGraphProjection, 16, 4; abi_size => 0, abi_reserved => 4, slot => 8, expression => 12);
    layout!(ZeGraphSortKey, 16, 4; abi_size => 0, abi_reserved => 4, expression => 8, descending => 12);
    layout!(ZeGraphParameter, 24, 4; abi_size => 0, abi_reserved => 4, name => 8, kinds => 16, reserved => 20);
    layout!(ZeGraphParameterValue, 24, 4; abi_size => 0, abi_reserved => 4, name => 8, value => 16, reserved => 20);
    layout!(ZeGraphSearchOptions, 64, 8; abi_size => 0, abi_reserved => 4, graph_profile => 8, graph_ef => 12, graph_seed => 16, lexical_flags => 24, rescore => 28, has_alpha => 32, rules_enabled => 36, alpha => 40, has_max_rounds => 48, reserved => 52, max_rounds => 56);
    layout!(ZeGraphSearch, 96, 8; abi_size => 0, abi_reserved => 4, kind => 8, call_id => 12, vector => 16, text => 24, k => 32, has_tier => 36, tier => 40, reserved => 44, eligible_set => 48, window => 56, node_slot => 64, score_slot => 68, vector_distance_slot => 72, lexical_score_slot => 80, options => 88);
    layout!(ZeGraphOperator, 192, 8; abi_size => 0, abi_reserved => 4, kind => 8, entity_kind => 12, inputs => 16, predicate => 24, source_slot => 32, node_slot => 36, relationship_slot => 40, set_slot => 44, direction => 48, pattern => 52, name => 56, has_name => 64, key_expression => 68, node_id => 72, relationship_id => 88, relationship_types => 104, path_min => 112, path_max => 116, edge_predicate => 120, edge_slot => 128, search => 132, projections => 136, aggregates => 144, sort_keys => 152, mutations => 160, offset => 168, limit => 176, has_limit => 184, reserved => 188);
    layout!(ZeGraphMutation, 56, 4; abi_size => 0, abi_reserved => 4, kind => 8, output => 12, entity => 16, value => 20, source => 24, target => 28, present => 32, detach => 36, name => 40, labels => 48);
    layout!(ZeGraphPlan, 184, 8; abi_size => 0, abi_reserved => 4, root => 8, reserved => 12, operators => 16, operator_count => 24, expressions => 32, expression_count => 40, inputs => 48, input_count => 56, expression_children => 64, expression_child_count => 72, projections => 80, projection_count => 88, sort_keys => 96, sort_key_count => 104, mutations => 112, mutation_count => 120, parameters => 128, parameter_count => 136, searches => 144, search_count => 152, eager_searches => 160, eager_search_count => 168, pool => 176);
    layout!(ZeGraphQueryOptions, 40, 8; abi_size => 0, abi_reserved => 4, query_tower => 8, alignment_digest => 16, limits => 32);
    layout!(ZeGraphQueryRequest, 56, 8; abi_size => 0, abi_reserved => 4, plan => 8, parameters => 16, parameter_count => 24, parameter_pool => 32, options => 40, control => 48);
    layout!(ZeGraphRelationshipType, 32, 8; abi_size => 0, abi_reserved => 4, name => 8, on_delete => 24, reserved => 28);
    layout!(ZeGraphOpenRequest, 64, 8; abi_size => 0, abi_reserved => 4, path => 8, mode => 24, tokenizer_profile => 28, document_tower => 32, reader_drain_timeout_ms => 40, max_resident_bytes => 48, control => 56);
    layout!(ZeGraphCypherRequest, 72, 8; abi_size => 0, abi_reserved => 4, query => 8, parameters => 24, parameter_count => 32, parameter_pool => 40, options => 48, control => 56, compile_limits => 64);
    layout!(ZeGraphGetNodesRequest, 48, 8; abi_size => 0, abi_reserved => 4, ids => 8, id_count => 16, include_text => 24, include_vector => 28, control => 32, limits => 40);
    layout!(ZeGraphGetRelsRequest, 40, 8; abi_size => 0, abi_reserved => 4, ids => 8, id_count => 16, control => 24, limits => 32);
    layout!(ZeGraphReceipt, 72, 8; abi_size => 0, abi_reserved => 4, item => 8, entity_kind => 12, disposition => 16, deleted => 20, node => 24, relationship => 40, revision => 56, generation => 64);
    layout!(ZeGraphColumn, 24, 4; abi_size => 0, abi_reserved => 4, name => 8, kinds => 16, reserved => 20);
    layout!(ZeGraphDiagnostic, 48, 4; abi_size => 0, abi_reserved => 4, code => 8, reserved => 12, operator_index => 16, has_source_span => 24, source_reserved => 28, source_span => 32, message => 40);
    layout!(ZeGraphWorkCounter, 24, 8; abi_size => 0, abi_reserved => 4, kind => 8, reserved => 12, value => 16);
    layout!(ZeGraphSearchReport, 144, 8; abi_size => 0, abi_reserved => 4, call_id => 8, kind => 12, generation => 16, has_requested_tier => 24, requested_tier => 28, has_actual_tier => 32, actual_tier => 36, precision => 40, coverage => 44, vector_leg => 48, lexical_leg => 52, has_document_epoch => 56, has_query_epoch => 60, has_tokenizer_epoch => 64, cross_score_complete => 68, document_epoch => 72, query_epoch => 80, tokenizer_epoch => 88, effective_alpha => 96, normalization_version => 104, rules_version => 108, candidate_count => 112, cross_scored_count => 120, fallback_count => 128, work => 136);
    layout!(ZeGraphResponse, 296, 8; abi_size => 0, abi_reserved => 4, owner_token => 8, disposition => 16, has_admitted_generation => 20, admitted_generation => 24, has_changed_generation => 32, reserved => 36, changed_generation => 40, row_count => 48, columns => 56, column_count => 64, cells => 72, cell_count => 80, pool => 88, receipts => 224, receipt_count => 232, reports => 240, report_count => 248, diagnostics => 256, diagnostic_count => 264, work => 272, work_count => 280, global_work => 288);
    layout!(ZeGraphWorkLimit, 24, 8; abi_size => 0, abi_reserved => 4, kind => 8, reserved => 12, limit => 16);
    layout!(ZeGraphQueryLimits, 40, 8; abi_size => 0, abi_reserved => 4, has_query_bytes => 8, reserved => 12, query_bytes => 16, work => 24, work_count => 32);
    layout!(ZeGraphCompileLimits, 40, 4; abi_size => 0, abi_reserved => 4, text_bytes => 8, tokens => 12, ast_nodes => 16, depth => 20, parameters => 24, columns => 28, list_depth => 32, path_hops => 36);
}

#[test]
fn every_graph_discriminant_has_the_frozen_value_and_width() {
    assert_eq!(size_of::<ZeGraphValueTag>(), 4);
    assert_eq!(ZeGraphValueTag::ZeGraphValueNull as u32, 0);
    assert_eq!(ZeGraphValueTag::ZeGraphValueBool as u32, 1);
    assert_eq!(ZeGraphValueTag::ZeGraphValueI64 as u32, 2);
    assert_eq!(ZeGraphValueTag::ZeGraphValueF64 as u32, 3);
    assert_eq!(ZeGraphValueTag::ZeGraphValueString as u32, 4);
    assert_eq!(ZeGraphValueTag::ZeGraphValueNode as u32, 5);
    assert_eq!(ZeGraphValueTag::ZeGraphValueRelationship as u32, 6);
    assert_eq!(ZeGraphValueTag::ZeGraphValueList as u32, 7);
    assert_eq!(size_of::<ZeGraphListKind>(), 4);
    assert_eq!(ZeGraphListKind::ZeGraphListQuery as u32, 0);
    assert_eq!(ZeGraphListKind::ZeGraphListBool as u32, 1);
    assert_eq!(ZeGraphListKind::ZeGraphListI64 as u32, 2);
    assert_eq!(ZeGraphListKind::ZeGraphListF64 as u32, 3);
    assert_eq!(ZeGraphListKind::ZeGraphListString as u32, 4);
    assert_eq!(ZeGraphListKind::ZeGraphListEmpty as u32, 5);
    assert_eq!(size_of::<ZeGraphEntityKind>(), 4);
    assert_eq!(ZeGraphEntityKind::ZeGraphEntityNode as u32, 0);
    assert_eq!(ZeGraphEntityKind::ZeGraphEntityRelationship as u32, 1);
    assert_eq!(size_of::<ZeGraphEndpointKind>(), 4);
    assert_eq!(ZeGraphEndpointKind::ZeGraphEndpointUnused as u32, 0);
    assert_eq!(ZeGraphEndpointKind::ZeGraphEndpointNode as u32, 1);
    assert_eq!(ZeGraphEndpointKind::ZeGraphEndpointLocal as u32, 2);
    assert_eq!(size_of::<ZeGraphBatchOperation>(), 4);
    assert_eq!(ZeGraphBatchOperation::ZeGraphBatchCreate as u32, 0);
    assert_eq!(ZeGraphBatchOperation::ZeGraphBatchPut as u32, 1);
    assert_eq!(ZeGraphBatchOperation::ZeGraphBatchDelete as u32, 2);
    assert_eq!(ZeGraphBatchOperation::ZeGraphBatchRecreate as u32, 3);
    assert_eq!(size_of::<ZeGraphExpressionKind>(), 4);
    assert_eq!(ZeGraphExpressionKind::ZeGraphExprLiteral as u32, 0);
    assert_eq!(ZeGraphExpressionKind::ZeGraphExprSlot as u32, 1);
    assert_eq!(ZeGraphExpressionKind::ZeGraphExprParameter as u32, 2);
    assert_eq!(ZeGraphExpressionKind::ZeGraphExprUnary as u32, 3);
    assert_eq!(ZeGraphExpressionKind::ZeGraphExprBinary as u32, 4);
    assert_eq!(ZeGraphExpressionKind::ZeGraphExprProperty as u32, 5);
    assert_eq!(ZeGraphExpressionKind::ZeGraphExprHasLabel as u32, 6);
    assert_eq!(ZeGraphExpressionKind::ZeGraphExprList as u32, 7);
    assert_eq!(ZeGraphExpressionKind::ZeGraphExprAggregate as u32, 8);
    assert_eq!(size_of::<ZeGraphUnaryOperation>(), 4);
    assert_eq!(ZeGraphUnaryOperation::ZeGraphUnaryNot as u32, 0);
    assert_eq!(ZeGraphUnaryOperation::ZeGraphUnaryPositive as u32, 1);
    assert_eq!(ZeGraphUnaryOperation::ZeGraphUnaryNegate as u32, 2);
    assert_eq!(ZeGraphUnaryOperation::ZeGraphUnaryIsNull as u32, 3);
    assert_eq!(ZeGraphUnaryOperation::ZeGraphUnaryIsNotNull as u32, 4);
    assert_eq!(ZeGraphUnaryOperation::ZeGraphUnarySize as u32, 5);
    assert_eq!(ZeGraphUnaryOperation::ZeGraphUnaryLabels as u32, 6);
    assert_eq!(
        ZeGraphUnaryOperation::ZeGraphUnaryRelationshipType as u32,
        7
    );
    assert_eq!(ZeGraphUnaryOperation::ZeGraphUnaryStoredText as u32, 8);
    assert_eq!(ZeGraphUnaryOperation::ZeGraphUnaryNodeIdText as u32, 9);
    assert_eq!(
        ZeGraphUnaryOperation::ZeGraphUnaryRelationshipIdText as u32,
        10
    );
    assert_eq!(size_of::<ZeGraphBinaryOperation>(), 4);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryAnd as u32, 0);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryOr as u32, 1);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryXor as u32, 2);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryEqual as u32, 3);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryNotEqual as u32, 4);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryLess as u32, 5);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryLessEqual as u32, 6);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryGreater as u32, 7);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryGreaterEqual as u32, 8);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryAdd as u32, 9);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinarySubtract as u32, 10);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryMultiply as u32, 11);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryDivide as u32, 12);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryRemainder as u32, 13);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryStartsWith as u32, 14);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryEndsWith as u32, 15);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryContains as u32, 16);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryIn as u32, 17);
    assert_eq!(ZeGraphBinaryOperation::ZeGraphBinaryIndex as u32, 18);
    assert_eq!(size_of::<ZeGraphAggregateOperation>(), 4);
    assert_eq!(ZeGraphAggregateOperation::ZeGraphAggregateCount as u32, 0);
    assert_eq!(ZeGraphAggregateOperation::ZeGraphAggregateCollect as u32, 1);
    assert_eq!(size_of::<ZeGraphTier>(), 4);
    assert_eq!(ZeGraphTier::ZeGraphTierAuto as u32, 0);
    assert_eq!(ZeGraphTier::ZeGraphTierExact as u32, 1);
    assert_eq!(ZeGraphTier::ZeGraphTierScan as u32, 2);
    assert_eq!(ZeGraphTier::ZeGraphTierGraph as u32, 3);
    assert_eq!(size_of::<ZeGraphSearchKind>(), 4);
    assert_eq!(ZeGraphSearchKind::ZeGraphSearchVector as u32, 0);
    assert_eq!(ZeGraphSearchKind::ZeGraphSearchText as u32, 1);
    assert_eq!(ZeGraphSearchKind::ZeGraphSearchHybrid as u32, 2);
    assert_eq!(size_of::<ZeGraphOperatorKind>(), 4);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpUnit as u32, 0);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpJoin as u32, 1);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpDistinct as u32, 2);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpSort as u32, 3);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpScanNodes as u32, 4);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpEager as u32, 5);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpMutate as u32, 6);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpAggregate as u32, 7);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpSearch as u32, 8);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpOffsetLimit as u32, 9);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpLookupNode as u32, 10);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpLookupRelationship as u32, 11);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpLookupKey as u32, 12);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpExpand as u32, 13);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpBoundedExpand as u32, 14);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpOptionalApply as u32, 15);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpProject as u32, 16);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpWith as u32, 17);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpFilter as u32, 18);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpCollect as u32, 19);
    assert_eq!(ZeGraphOperatorKind::ZeGraphOpEligibleSet as u32, 20);
    assert_eq!(size_of::<ZeGraphMutationKind>(), 4);
    assert_eq!(ZeGraphMutationKind::ZeGraphMutationCreateNode as u32, 0);
    assert_eq!(
        ZeGraphMutationKind::ZeGraphMutationCreateRelationship as u32,
        1
    );
    assert_eq!(ZeGraphMutationKind::ZeGraphMutationRemoveProperty as u32, 2);
    assert_eq!(ZeGraphMutationKind::ZeGraphMutationSetLabel as u32, 3);
    assert_eq!(ZeGraphMutationKind::ZeGraphMutationDelete as u32, 4);
    assert_eq!(ZeGraphMutationKind::ZeGraphMutationSetProperty as u32, 5);
    assert_eq!(size_of::<ZeGraphOpenMode>(), 4);
    assert_eq!(ZeGraphOpenMode::ZeGraphOpenCreate as u32, 0);
    assert_eq!(ZeGraphOpenMode::ZeGraphOpenReadWrite as u32, 1);
    assert_eq!(ZeGraphOpenMode::ZeGraphOpenReadOnly as u32, 2);
    assert_eq!(size_of::<ZeGraphDisposition>(), 4);
    assert_eq!(
        ZeGraphDisposition::ZeGraphDispositionNotApplicable as u32,
        0
    );
    assert_eq!(ZeGraphDisposition::ZeGraphDispositionNotCommitted as u32, 1);
    assert_eq!(ZeGraphDisposition::ZeGraphDispositionCommitted as u32, 2);
    assert_eq!(ZeGraphDisposition::ZeGraphDispositionReplayed as u32, 3);
    assert_eq!(ZeGraphDisposition::ZeGraphDispositionNoOp as u32, 4);
    assert_eq!(
        ZeGraphDisposition::ZeGraphDispositionIndeterminate as u32,
        5
    );
    assert_eq!(size_of::<ZeGraphScorePrecision>(), 4);
    assert_eq!(
        ZeGraphScorePrecision::ZeGraphPrecisionNotApplicable as u32,
        0
    );
    assert_eq!(ZeGraphScorePrecision::ZeGraphPrecisionOriginal as u32, 1);
    assert_eq!(ZeGraphScorePrecision::ZeGraphPrecisionQuantized as u32, 2);
    assert_eq!(ZeGraphScorePrecision::ZeGraphPrecisionMixed as u32, 3);
    assert_eq!(size_of::<ZeGraphCandidateCoverage>(), 4);
    assert_eq!(ZeGraphCandidateCoverage::ZeGraphCoverageExact as u32, 0);
    assert_eq!(
        ZeGraphCandidateCoverage::ZeGraphCoverageApproximate as u32,
        1
    );
    assert_eq!(size_of::<ZeGraphLegState>(), 4);
    assert_eq!(ZeGraphLegState::ZeGraphLegNotRequested as u32, 0);
    assert_eq!(ZeGraphLegState::ZeGraphLegNonempty as u32, 1);
    assert_eq!(ZeGraphLegState::ZeGraphLegNoIndexedPopulation as u32, 2);
    assert_eq!(ZeGraphLegState::ZeGraphLegNoEligibleMembers as u32, 3);
    assert_eq!(ZeGraphLegState::ZeGraphLegNoQueryMatches as u32, 4);
    assert_eq!(size_of::<ZeGraphWorkKind>(), 4);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkOperatorRows as u32, 0);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkAdjacencyEntries as u32, 1);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkExpressions as u32, 2);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkHashProbes as u32, 3);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkCompletedRows as u32, 4);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkCompletedBytes as u32, 5);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkPreparedPayloadBytes as u32, 6);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkCompletedAbiBytes as u32, 7);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkVectorCoordinates as u32, 8);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkVectorBytes as u32, 9);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkLexicalPostings as u32, 10);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkLexicalBlocks as u32, 11);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkSearchInvocations as u32, 12);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkLookups as u32, 13);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkScans as u32, 14);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkPaths as u32, 15);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkRowsIn as u32, 16);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkRowsOut as u32, 17);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkJoinProbes as u32, 18);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkGroupKeys as u32, 19);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkEligibilityEntries as u32, 20);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkCopiedBytes as u32, 21);
    assert_eq!(ZeGraphWorkKind::ZeGraphWorkPeakOwnedBytes as u32, 22);
}

#[test]
fn ze241_new_entry_requests_keep_frozen_layouts_and_signatures() {
    layout!(ZeGraphQueryRequest,56,8; abi_size=>0,abi_reserved=>4,plan=>8,parameters=>16,parameter_count=>24,parameter_pool=>32,options=>40,control=>48);
    layout!(ZeGraphGetNodesRequest,48,8; abi_size=>0,abi_reserved=>4,ids=>8,id_count=>16,include_text=>24,include_vector=>28,control=>32,limits=>40);
    layout!(ZeGraphGetRelsRequest,40,8; abi_size=>0,abi_reserved=>4,ids=>8,id_count=>16,control=>24,limits=>32);
    let _: extern "C" fn(
        ZeHandle,
        *const ZeGraphQueryRequest,
        *mut ZeGraphResponse,
    ) -> ZeErrorCode = ze_store_graph_query;
    let _: extern "C" fn(
        ZeHandle,
        *const ZeGraphGetNodesRequest,
        *mut ZeGraphResponse,
    ) -> ZeErrorCode = ze_store_get_nodes;
    let _: extern "C" fn(
        ZeHandle,
        *const ZeGraphGetRelsRequest,
        *mut ZeGraphResponse,
    ) -> ZeErrorCode = ze_store_get_relationships;
}
