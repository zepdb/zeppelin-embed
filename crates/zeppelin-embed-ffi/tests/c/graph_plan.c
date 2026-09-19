#include "zeppelin_graph_contracts.h"
int main(void) {
    ZeGraphExpression expressions[] = {{.abi_size = sizeof(ZeGraphExpression),
        .kind = ZE_GRAPH_EXPR_LITERAL, .value = 0}};
    ZeGraphSearch search = {.abi_size = sizeof(ZeGraphSearch),
        .kind = ZE_GRAPH_SEARCH_HYBRID, .vector = {1, 0}, .text = {1, 1},
        .k = 2, .has_tier = 1, .tier = ZE_GRAPH_TIER_AUTO,
        .eligible_set = {1, 4000}, .node_slot = 21, .score_slot = 22,
        .vector_distance_slot = {1, 23}, .lexical_score_slot = {1, 24}};
    ZeGraphOperator operators[] = {{.abi_size = sizeof(ZeGraphOperator),
        .kind = ZE_GRAPH_OP_SEARCH, .search = 0},
        {.abi_size = sizeof(ZeGraphOperator), .kind = ZE_GRAPH_OP_BOUNDED_EXPAND,
         .source_slot = 21, .node_slot = 31, .relationship_slot = 32,
         .relationship_types = {0, 2}, .edge_predicate = {1, 3},
         .edge_slot = 90, .path_min = 1, .path_max = 16, .pattern = 7}};
    ZeGraphPlan plan = {.abi_size = sizeof(ZeGraphPlan), .root = 1,
        .operators = operators, .operator_count = 2,
        .expressions = expressions, .expression_count = 1,
        .searches = &search, .search_count = 1};
    ZeGraphQueryRequest query = {.abi_size = sizeof(ZeGraphQueryRequest), .plan = &plan};
    return !(query.plan->searches[0].has_tier == 1 && search.tier == ZE_GRAPH_TIER_AUTO &&
        search.eligible_set.present == 1 && search.vector_distance_slot.index == 23 &&
        operators[1].relationship_types.count == 2 && operators[1].edge_predicate.present == 1);
}
