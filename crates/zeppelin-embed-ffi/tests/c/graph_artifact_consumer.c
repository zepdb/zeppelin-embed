#include "zeppelin_embed.h"
#include "zeppelin_graph_contracts.h"
#include <assert.h>
#include <stdio.h>
#include <string.h>
#define SIZED(T) ((T){.abi_size = sizeof(T)})
static ZeGraphResponse query(ZeGraphHandle handle, const char *text) {
    ZeGraphCypherRequest q = SIZED(ZeGraphCypherRequest);
    q.query = (ZeGraphBytes){(const uint8_t *)text, strlen(text)};
    ZeGraphResponse r = SIZED(ZeGraphResponse);
    r.pool.abi_size = sizeof(r.pool);
    assert(ze_graph_cypher(handle, &q, &r) == ZE_OK);
    return r;
}
int main(int argc, char **argv) {
    assert(argc == 2);
    char path[4096];
    assert(snprintf(path, sizeof(path), "%s/legacy", argv[1]) < (int)sizeof(path));
    ZeOpenRequest legacy = SIZED(ZeOpenRequest);
    legacy.path = (const uint8_t *)path; legacy.path_len = strlen(path);
    legacy.commit_tier = 1; legacy.reader_drain_timeout_ms = 250;
    legacy.max_resident_bytes = UINT64_MAX; legacy.max_temp_bytes = UINT64_MAX;
    uint64_t core = 0;
    assert(ze_open(&legacy, &core) == ZE_OK);
    assert(ze_close(core) == ZE_OK);
    assert(snprintf(path, sizeof(path), "%s/graph", argv[1]) < (int)sizeof(path));
    ZeGraphOpenRequest open = SIZED(ZeGraphOpenRequest);
    open.path = (ZeGraphBytes){(const uint8_t *)path, strlen(path)};
    open.reader_drain_timeout_ms = 250; open.max_resident_bytes = 256ULL << 20;
    ZeGraphHandle graph = {0};
    assert(ze_graph_open(&open, &graph) == ZE_OK);
    ZeGraphNode node = SIZED(ZeGraphNode);
    ZeGraphValuePool pool = SIZED(ZeGraphValuePool);
    pool.nodes = &node; pool.node_count = 1;
    ZeGraphBatchItem item = SIZED(ZeGraphBatchItem);
    item.revision = 1; item.has_image = 1;
    item.source.abi_size = sizeof(item.source); item.target.abi_size = sizeof(item.target);
    ZeGraphBatchRequest batch = SIZED(ZeGraphBatchRequest);
    batch.items = &item; batch.item_count = 1; batch.pool = &pool;
    ZeGraphResponse r = SIZED(ZeGraphResponse); r.pool.abi_size = sizeof(r.pool);
    assert(ze_graph_apply(graph, &batch, &r) == ZE_OK);
    assert(r.has_changed_generation == 1 && r.changed_generation == 1 && r.receipt_count == 1);
    assert(ze_graph_response_free(&r) == ZE_OK);
    r = query(graph, "CREATE (:Doc {title: 'alpha'})");
    assert(r.disposition == 2 && r.changed_generation == 2 && r.row_count == 0);
    assert(ze_graph_response_free(&r) == ZE_OK);
    r = query(graph, "MATCH (n:Doc) RETURN n.title AS title");
    assert(r.has_admitted_generation == 1 && r.admitted_generation == 2);
    assert(r.row_count == 1 && r.column_count == 1 && r.cell_count == 1);
    const ZeGraphValue *value = &r.pool.values[r.cells[0]];
    assert(value->tag == 4 && value->range.count == 5);
    assert(memcmp(r.pool.bytes + value->range.start, "alpha", 5) == 0);
    assert(ze_graph_response_free(&r) == ZE_OK);
    assert(ze_graph_close(graph) == ZE_OK);
    puts("graph artifact consumer: legacy open/close, apply, Cypher write/read/free/close PASS");
    return 0;
}
