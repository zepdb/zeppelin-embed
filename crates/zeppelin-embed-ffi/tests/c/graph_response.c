#include "zeppelin_graph_contracts.h"
int main(void) {
    ZeGraphOpenRequest open = {.abi_size = sizeof(ZeGraphOpenRequest),
        .mode = ZE_GRAPH_OPEN_CREATE, .tokenizer_profile = 0,
        .max_resident_bytes = UINT64_C(268435456)};
    ZeGraphCypherRequest cypher = {.abi_size = sizeof(ZeGraphCypherRequest),
        .query = {(const uint8_t *)"RETURN 1", 8}};
    ZeNodeId ids[] = {{UINT64_MAX, 9}};
    ZeGraphGetNodesRequest get = {.abi_size = sizeof(ZeGraphGetNodesRequest),
        .ids = ids, .id_count = 1, .include_text = 1};
    ZeGraphSearchReport reports[] = {{.abi_size = sizeof(ZeGraphSearchReport),
        .call_id = 7, .kind = ZE_GRAPH_SEARCH_HYBRID,
        .precision = ZE_GRAPH_PRECISION_ORIGINAL, .coverage = ZE_GRAPH_COVERAGE_APPROXIMATE,
        .vector_leg = ZE_GRAPH_LEG_NO_ELIGIBLE_MEMBERS,
        .lexical_leg = ZE_GRAPH_LEG_NONEMPTY, .effective_alpha = 0.0}};
    ZeGraphReceipt receipts[] = {{.abi_size = sizeof(ZeGraphReceipt),
        .item = 0, .entity_kind = ZE_GRAPH_ENTITY_NODE,
        .disposition = ZE_GRAPH_DISPOSITION_REPLAYED,
        .node = {1, 9}, .generation = 3}};
    ZeGraphResponse response = {.abi_size = sizeof(ZeGraphResponse),
        .has_admitted_generation = 1, .admitted_generation = 7,
        .disposition = ZE_GRAPH_DISPOSITION_NO_OP,
        .reports = reports, .report_count = 1, .receipts = receipts, .receipt_count = 1};
    return !(open.document_tower == NULL && cypher.query.count == 8 && get.ids[0].high == UINT64_MAX &&
        response.has_changed_generation == 0 && response.admitted_generation == 7 &&
        reports[0].precision == ZE_GRAPH_PRECISION_ORIGINAL &&
        reports[0].coverage == ZE_GRAPH_COVERAGE_APPROXIMATE && receipts[0].generation == 3);
}
