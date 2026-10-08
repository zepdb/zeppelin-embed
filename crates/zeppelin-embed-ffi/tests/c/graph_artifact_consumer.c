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
    /* Same two-node/relationship fixture as the installed Swift consumer. */
    const uint8_t bytes[] = "DocalphatitleLINKfixtureabr";
    ZeGraphValue title = SIZED(ZeGraphValue);
    title.tag = 4; title.range = (ZeGraphRange){3, 5};
    ZeGraphProperty property = SIZED(ZeGraphProperty);
    property.name = (ZeGraphRange){8, 5}; property.value = 0;
    ZeGraphRange label = {0, 3};
    ZeGraphNode nodes[2] = {SIZED(ZeGraphNode), SIZED(ZeGraphNode)};
    nodes[0].labels = (ZeGraphRange){0, 1}; nodes[1].labels = (ZeGraphRange){0, 1};
    nodes[0].properties = (ZeGraphRange){0, 1};
    ZeGraphRelationship relationship = SIZED(ZeGraphRelationship);
    relationship.relationship_type = (ZeGraphRange){13, 4};
    ZeGraphValuePool pool = SIZED(ZeGraphValuePool);
    pool.bytes = bytes; pool.byte_count = sizeof(bytes) - 1;
    pool.values = &title; pool.value_count = 1;
    pool.properties = &property; pool.property_count = 1;
    pool.names = &label; pool.name_count = 1;
    pool.nodes = nodes; pool.node_count = 2;
    pool.relationships = &relationship; pool.relationship_count = 1;
    ZeGraphBatchItem items[3] = {SIZED(ZeGraphBatchItem), SIZED(ZeGraphBatchItem), SIZED(ZeGraphBatchItem)};
    for (size_t i = 0; i < 3; ++i) {
        items[i].revision = 1; items[i].has_image = 1;
        items[i].namespace_name = (ZeGraphRange){17, 7};
        items[i].key = (ZeGraphRange){24 + (uint32_t)i, 1};
        items[i].source.abi_size = sizeof(items[i].source);
        items[i].target.abi_size = sizeof(items[i].target);
    }
    items[1].image = 1;
    items[2].entity_kind = 1;
    items[2].source.kind = 2; items[2].source.local_item = 0;
    items[2].target.kind = 2; items[2].target.local_item = 1;
    ZeGraphBatchRequest batch = SIZED(ZeGraphBatchRequest);
    batch.items = items; batch.item_count = 3; batch.pool = &pool;
    ZeGraphResponse r = SIZED(ZeGraphResponse); r.pool.abi_size = sizeof(r.pool);
    assert(ze_graph_apply(graph, &batch, &r) == ZE_OK);
    /* One store counter: enable_graph commits 1; the first graph write and its reads use 2. */
    assert(r.has_changed_generation == 1 && r.changed_generation == 2 && r.receipt_count == 3);
    assert(ze_graph_response_free(&r) == ZE_OK);
    r = query(graph, "MATCH (n:Doc)-[:LINK]->(m) RETURN n.title AS title");
    assert(r.has_admitted_generation == 1 && r.admitted_generation == 2);
    assert(r.row_count == 1 && r.column_count == 1 && r.cell_count == 1);
    const ZeGraphValue *value = &r.pool.values[r.cells[0]];
    assert(value->tag == 4 && value->range.count == 5);
    assert(memcmp(r.pool.bytes + value->range.start, "alpha", 5) == 0);
    assert(ze_graph_response_free(&r) == ZE_OK);
    assert(ze_graph_close(graph) == ZE_OK);
    open.mode = 1;
    assert(ze_graph_open(&open, &graph) == ZE_OK);
    ZeGraphValue parameter_value = SIZED(ZeGraphValue);
    parameter_value.tag = 2; parameter_value.integer = 42;
    ZeGraphValuePool parameters = SIZED(ZeGraphValuePool);
    parameters.bytes = (const uint8_t *)"number"; parameters.byte_count = 6;
    parameters.values = &parameter_value; parameters.value_count = 1;
    ZeGraphParameterValue parameter = SIZED(ZeGraphParameterValue);
    parameter.name = (ZeGraphRange){0, 6}; parameter.value = 0;
    const char *text = "MATCH (n:Doc)-[:LINK]->(m) RETURN n.title, $number";
    ZeGraphCypherRequest q = SIZED(ZeGraphCypherRequest);
    q.query = (ZeGraphBytes){(const uint8_t *)text, strlen(text)};
    q.parameters = &parameter; q.parameter_count = 1; q.parameter_pool = &parameters;
    r = SIZED(ZeGraphResponse); r.pool.abi_size = sizeof(r.pool);
    assert(ze_graph_cypher(graph, &q, &r) == ZE_OK);
    assert(r.admitted_generation == 2 && r.row_count == 1 && r.column_count == 2);
    value = &r.pool.values[r.cells[0]];
    assert(value->tag == 4 && value->range.count == 5);
    assert(memcmp(r.pool.bytes + value->range.start, "alpha", 5) == 0);
    value = &r.pool.values[r.cells[1]];
    assert(value->tag == 2 && value->integer == 42);
    assert(ze_graph_response_free(&r) == ZE_OK);
    assert(ze_graph_close(graph) == ZE_OK);
    /* The shipping structured C entry and gets are present on main. */
    open.mode = 1;
    assert(ze_graph_open(&open, &graph) == ZE_OK);
    ZeGraphOperator scan = SIZED(ZeGraphOperator);
    scan.kind = 4; scan.node_slot = 7;
    ZeGraphValuePool structured_pool = SIZED(ZeGraphValuePool);
    ZeGraphPlan plan = SIZED(ZeGraphPlan);
    plan.operators = &scan; plan.operator_count = 1; plan.pool = &structured_pool;
    ZeGraphQueryRequest structured = SIZED(ZeGraphQueryRequest); structured.plan = &plan;
    r = SIZED(ZeGraphResponse); r.pool.abi_size = sizeof(r.pool);
    assert(ze_graph_query(graph, &structured, &r) == ZE_OK);
    assert(r.row_count == 2 && r.pool.node_count == 2);
    ZeNodeId ids[3] = {r.pool.nodes[0].id, {UINT64_MAX, UINT64_MAX}, r.pool.nodes[0].id};
    assert(ze_graph_response_free(&r) == ZE_OK);
    ZeGraphGetNodesRequest get = SIZED(ZeGraphGetNodesRequest);
    get.ids = ids; get.id_count = 3;
    assert(ze_graph_get_nodes(graph, &get, &r) == ZE_OK);
    assert(r.row_count == 3);
    assert(r.pool.values[r.cells[0]].tag == 5 && r.pool.values[r.cells[1]].tag == 0);
    assert(r.pool.values[r.cells[0]].entity_index == r.pool.values[r.cells[2]].entity_index);
    ZeGraphResources resources = SIZED(ZeGraphResources);
    assert(ze_graph_resources(graph, &resources) == ZE_OK);
    assert(resources.engine_peak_bytes >= resources.engine_bytes);
    assert(ze_graph_close(graph) == ZE_OK);
    assert(r.pool.values[r.cells[0]].tag == 5);
    assert(ze_graph_response_free(&r) == ZE_OK);
    /* ZE-72 graph-bindings-v1 completed/null/list/bag fixture. */
    open.mode = 1;
    assert(ze_graph_open(&open, &graph) == ZE_OK);
    r = query(graph, "RETURN null, '', [], [1,null,['nested']]");
    assert(r.row_count == 1 && r.column_count == 4);
    assert(r.pool.values[r.cells[0]].tag == 0);
    assert(r.pool.values[r.cells[1]].tag == 4 && r.pool.values[r.cells[1]].range.count == 0);
    value = &r.pool.values[r.cells[2]];
    assert(value->tag == 7 && value->list_kind == 0 && value->range.count == 0);
    value = &r.pool.values[r.cells[3]];
    assert(value->tag == 7 && value->list_kind == 0 && value->range.count == 3);
    const uint32_t *children = r.pool.children + value->range.start;
    assert(r.pool.values[children[0]].tag == 2 && r.pool.values[children[0]].integer == 1);
    assert(r.pool.values[children[1]].tag == 0);
    const ZeGraphValue *nested = &r.pool.values[children[2]];
    assert(nested->tag == 7 && nested->range.count == 1);
    const ZeGraphValue *text_value = &r.pool.values[r.pool.children[nested->range.start]];
    assert(text_value->tag == 4 && text_value->range.count == 6);
    assert(memcmp(r.pool.bytes + text_value->range.start, "nested", 6) == 0);
    assert(ze_graph_close(graph) == ZE_OK);
    /* Access the retained descriptor after close, then use the matching free. */
    assert(r.pool.values[r.cells[3]].range.count == 3);
    assert(ze_graph_response_free(&r) == ZE_OK);
    assert(ze_graph_open(&open, &graph) == ZE_OK);
    r = query(graph, "MATCH (n) RETURN null ORDER BY n");
    assert(r.row_count == 2 && r.cell_count == 2);
    assert(r.pool.values[r.cells[0]].tag == 0 && r.pool.values[r.cells[1]].tag == 0);
    assert(ze_graph_response_free(&r) == ZE_OK);
    r = query(graph, "MATCH (n:Missing) RETURN n");
    assert(r.row_count == 0 && r.cell_count == 0);
    assert(ze_graph_response_free(&r) == ZE_OK);
    assert(ze_graph_close(graph) == ZE_OK);
    puts("graph artifact consumer: legacy open/close, apply, Cypher relationship/parameter/read/free/close/reopen PASS");
    puts("ZE_GRAPH_INSTALLED_RECEIPT\t{\"executed\":[\"batch\",\"structured\",\"get\",\"cypher\"],\"resources\":true,\"artifact_kind\":\"graph-cypher\",\"exit_status\":0}");
    return 0;
}
