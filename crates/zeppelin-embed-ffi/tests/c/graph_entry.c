#include "zeppelin_graph_contracts.h"
ze_error_code (*open_graph)(const ZeGraphOpenRequest *, ZeGraphHandle *) = ze_graph_open;
ze_error_code (*close_graph)(ZeGraphHandle) = ze_graph_close;
ze_error_code (*apply_graph)(ZeGraphHandle, const ZeGraphBatchRequest *, ZeGraphResponse *) = ze_graph_apply;
ze_error_code (*cypher_graph)(ZeGraphHandle, const ZeGraphCypherRequest *, ZeGraphResponse *) = ze_graph_cypher;
ze_error_code (*free_graph)(ZeGraphResponse *) = ze_graph_response_free;
ze_error_code (*limited_cypher_graph)(ZeGraphHandle, const ZeGraphCypherRequest *, uint32_t, ZeGraphResponse *) = ze_graph_cypher_with_row_limit;

ze_error_code (*open_graph_rules)(const ZeGraphOpenRequest *, const ZeGraphRelationshipType *, size_t, ZeGraphHandle *) = ze_graph_open_with_relationship_types;

ze_error_code (*maintain_graph)(ZeGraphHandle, const ZeGraphControl *, ZeGraphMaintainReport *) = ze_graph_maintain;
ze_error_code (*graph_maintenance_policy)(ZeGraphHandle, const ZeGraphMaintenancePolicy *) = ze_graph_set_maintenance_policy;
_Static_assert(sizeof(ZeGraphMaintenancePolicy) == 16, "maintenance policy size");
_Static_assert(sizeof(ZeGraphMaintainReport) == 64, "maintenance report size");
