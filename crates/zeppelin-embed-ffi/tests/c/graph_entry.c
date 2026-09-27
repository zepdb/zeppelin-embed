#include "zeppelin_graph_contracts.h"
ze_error_code (*open_graph)(const ZeGraphOpenRequest *, ZeGraphHandle *) = ze_graph_open;
ze_error_code (*close_graph)(ZeGraphHandle) = ze_graph_close;
ze_error_code (*apply_graph)(ZeGraphHandle, const ZeGraphBatchRequest *, ZeGraphResponse *) = ze_graph_apply;
ze_error_code (*cypher_graph)(ZeGraphHandle, const ZeGraphCypherRequest *, ZeGraphResponse *) = ze_graph_cypher;
ze_error_code (*free_graph)(ZeGraphResponse *) = ze_graph_response_free;
ze_error_code (*limited_cypher_graph)(ZeGraphHandle, const ZeGraphCypherRequest *, uint32_t, ZeGraphResponse *) = ze_graph_cypher_with_row_limit;

ze_error_code (*open_graph_rules)(const ZeGraphOpenRequest *, const ZeGraphRelationshipType *, size_t, ZeGraphHandle *) = ze_graph_open_with_relationship_types;
