#include "zeppelin_graph_contracts.h"
#include <stdio.h>
#include <string.h>
#define CHECK(condition) do { if (!(condition)) { fprintf(stderr, "ZE399 C check failed at line %d\n", __LINE__); return 1; } } while (0)
int main(int argc, char **argv) {
    CHECK(argc == 2);
    ZeOpenRequest open = {.abi_size = sizeof(ZeOpenRequest),
        .path = (const uint8_t *)argv[1], .path_len = strlen(argv[1]),
        .commit_tier = 1, .reader_drain_timeout_ms = 250,
        .max_resident_bytes = 256 * 1024 * 1024, .max_temp_bytes = UINT64_MAX};
    ze_handle handle = 0;
    CHECK(ze_open(&open, &handle) == ZE_OK);
    ZeGenerationReport enabled = {.abi_size = sizeof(ZeGenerationReport)};
    CHECK(ze_store_enable_graph(handle, &enabled) == ZE_OK);
    const uint8_t bytes[] = "docsnodeorchard";
    ZeGraphNode node = {.abi_size = sizeof(ZeGraphNode), .has_text = 1, .text = {8, 7}};
    ZeGraphValuePool pool = {.abi_size = sizeof(ZeGraphValuePool),
        .bytes = bytes, .byte_count = sizeof(bytes) - 1, .nodes = &node, .node_count = 1};
    ZeGraphBatchItem item = {.abi_size = sizeof(ZeGraphBatchItem),
        .namespace_name = {0, 4}, .key = {4, 4}, .revision = 1, .has_image = 1,
        .source = {.abi_size = sizeof(ZeGraphEndpoint)},
        .target = {.abi_size = sizeof(ZeGraphEndpoint)}};
    ZeGraphBatchRequest graph = {.abi_size = sizeof(ZeGraphBatchRequest),
        .items = &item, .item_count = 1, .pool = &pool};
    ZeStoreGraphDocument document = {.abi_size = sizeof(ZeStoreGraphDocument),
        .has_id = 1, .id = {1, 91}, .timestamp = 9,
        .metadata = (const uint8_t *)"raw", .metadata_len = 3};
    ZeStoreGraphBatchRequestV2 batch = {.abi_size = sizeof(ZeStoreGraphBatchRequestV2),
        .graph = &graph, .documents = &document, .document_count = 1};
    ZeGraphResponse applied = {.abi_size = sizeof(ZeGraphResponse)};
    CHECK(ze_store_graph_apply_v2(handle, &batch, &applied) == ZE_OK);
    CHECK(applied.receipt_count == 1 && applied.receipts[0].node.high == 1 && applied.receipts[0].node.low == 91);
    CHECK(applied.changed_generation == enabled.generation + 1);
    CHECK(ze_graph_response_free(&applied) == ZE_OK);
    for (int phase = 0; phase < 2; ++phase) {
        ZeQueryRequest query = {.abi_size = sizeof(ZeQueryRequest),
            .text = (const uint8_t *)"orchard", .text_len = 7, .k = 1, .thread_budget = 1};
        ZeQueryResult found = {.abi_size = sizeof(ZeQueryResult)};
        CHECK(ze_query(handle, &query, &found) == ZE_OK);
        CHECK(found.hit_count == 1 && found.hits[0].doc_id.high == 1 && found.hits[0].doc_id.low == 91);
        CHECK(ze_query_result_free(&found) == ZE_OK);
        const uint8_t text[] = "MATCH (n:Document) RETURN n";
        ZeGraphCypherRequest cypher = {.abi_size = sizeof(ZeGraphCypherRequest), .query = {text, sizeof(text) - 1}};
        ZeGraphResponse visible = {.abi_size = sizeof(ZeGraphResponse)};
        CHECK(ze_store_cypher(handle, &cypher, &visible) == ZE_OK);
        CHECK(visible.row_count == 1 && visible.pool.nodes[0].id.high == 1 && visible.pool.nodes[0].id.low == 91);
        CHECK(ze_graph_response_free(&visible) == ZE_OK);
        ZeGetRequest get = {.abi_size = sizeof(ZeGetRequest), .ids = &document.id, .id_count = 1, .include_text = 1, .include_metadata = 1};
        ZeGetResult stored = {.abi_size = sizeof(ZeGetResult)};
        CHECK(ze_get(handle, &get, &stored) == ZE_OK);
        CHECK(stored.documents[0].timestamp == 9 && stored.documents[0].metadata_len == 3);
        CHECK(memcmp(stored.documents[0].metadata, "raw", 3) == 0);
        CHECK(ze_get_result_free(&stored) == ZE_OK);
        CHECK(ze_close(handle) == ZE_OK);
        if (phase == 0) CHECK(ze_open(&open, &handle) == ZE_OK);
    }
    return 0;
}
