#include "zeppelin_graph_contracts.h"
int main(void) {
    const uint8_t bytes[] = "peoplealicekindknows";
    const ZeGraphRange labels[] = {{11, 4}};
    ZeGraphValue values[] = {{.abi_size = sizeof(ZeGraphValue),
        .tag = ZE_GRAPH_VALUE_LIST, .list_kind = ZE_GRAPH_LIST_EMPTY}};
    ZeGraphProperty properties[] = {{.abi_size = sizeof(ZeGraphProperty),
        .name = {11, 4}, .value = 0}};
    ZeGraphNode nodes[] = {{.abi_size = sizeof(ZeGraphNode),
        .has_text = 1, .text = {19, 0}, .labels = {0, 1}, .properties = {0, 1}}};
    ZeGraphValuePool pool = {.abi_size = sizeof(ZeGraphValuePool),
        .values = values, .value_count = 1, .bytes = bytes,
        .byte_count = sizeof(bytes) - 1, .names = labels, .name_count = 1,
        .properties = properties, .property_count = 1, .nodes = nodes, .node_count = 1};
    ZeGraphBatchItem items[] = {{.abi_size = sizeof(ZeGraphBatchItem),
        .entity_kind = ZE_GRAPH_ENTITY_NODE, .operation = ZE_GRAPH_BATCH_CREATE,
        .namespace_name = {0, 6}, .key = {6, 5}, .revision = 1,
        .has_image = 1, .source = {.abi_size = sizeof(ZeGraphEndpoint)},
        .target = {.abi_size = sizeof(ZeGraphEndpoint)}}};
    ZeGraphBatchRequest request = {.abi_size = sizeof(ZeGraphBatchRequest),
        .items = items, .item_count = 1, .pool = &pool};
    ZeGraphEndpoint local = {.abi_size = sizeof(ZeGraphEndpoint),
        .kind = ZE_GRAPH_ENDPOINT_LOCAL, .local_item = 0};
    ZeGraphEndpoint existing = {.abi_size = sizeof(ZeGraphEndpoint),
        .kind = ZE_GRAPH_ENDPOINT_NODE, .node = {UINT64_MAX, 7}};
    return !(request.item_count == 1 && local.kind != existing.kind &&
        existing.node.high == UINT64_MAX && nodes[0].has_text == 1 &&
        nodes[0].text.count == 0 && values[0].range.count == 0);
}
