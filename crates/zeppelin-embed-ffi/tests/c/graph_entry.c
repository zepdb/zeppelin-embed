#include "zeppelin_graph_contracts.h"
ze_error_code (*close_graph)(ze_handle) = ze_close;
ze_error_code (*apply_graph)(ze_handle, const ZeGraphBatchRequest *, ZeGraphResponse *) = ze_store_graph_apply;
ze_error_code (*cypher_graph)(ze_handle, const ZeGraphCypherRequest *, ZeGraphResponse *) = ze_store_cypher;
ze_error_code (*free_graph)(ZeGraphResponse *) = ze_graph_response_free;
ze_error_code (*limited_cypher_graph)(ze_handle, const ZeGraphCypherRequest *, uint32_t, ZeGraphResponse *) = ze_store_cypher_with_row_limit;

ze_error_code (*open_graph_rules)(const ZeGraphOpenRequest *, const ZeGraphRelationshipType *, size_t, ze_handle *) = ze_store_create_with_relationship_types;

ze_error_code (*maintain_graph)(ze_handle, const ZeGraphControl *, ZeGraphMaintainReport *) = ze_store_graph_maintain;
ze_error_code (*graph_maintenance_policy)(ze_handle, const ZeGraphMaintenancePolicy *) = ze_store_set_graph_maintenance_policy;
_Static_assert(sizeof(ZeGraphMaintenancePolicy) == 16, "maintenance policy size");
_Static_assert(sizeof(ZeGraphMaintainReport) == 64, "maintenance report size");

ze_error_code (*query_graph)(ze_handle, const ZeGraphQueryRequest *, ZeGraphResponse *) = ze_store_graph_query;
ze_error_code (*get_graph_nodes)(ze_handle, const ZeGraphGetNodesRequest *, ZeGraphResponse *) = ze_store_get_nodes;
ze_error_code (*get_graph_relationships)(ze_handle, const ZeGraphGetRelsRequest *, ZeGraphResponse *) = ze_store_get_relationships;
ze_error_code (*enable_graph)(ze_handle, ZeGenerationReport *) = ze_store_enable_graph;
