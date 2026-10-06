#include "zeppelin_graph_contracts.h"
#include <stdio.h>
#include <string.h>

#define S(T) .abi_size = sizeof(T)
#define CHECK(x) do { if (!(x)) { fprintf(stderr, "failed: %s:%d: %s\n", __FILE__, __LINE__, #x); return 1; } } while (0)

/* Independently query each shape through Unit -> With -> Project. */
static int query(ZeGraphHandle handle, unsigned shape, ZeGraphResponse *response) {
    unsigned char bytes[] = "phello";
    ZeGraphValue values[] = {
        {S(ZeGraphValue), .tag = 4, .range = {1, 5}},
        {S(ZeGraphValue), .tag = 2, .integer = 3},
        {S(ZeGraphValue), .tag = 3, .floating = 4.5},
        {S(ZeGraphValue), .tag = 7},
        {S(ZeGraphValue), .tag = 7, .range = {0, 4}},
        {S(ZeGraphValue), .tag = 7, .range = {4, 2}},
        {S(ZeGraphValue), .tag = 7, .range = {4, 2}}
    };
    uint32_t children[] = {0, 1, 6, 3, 1, 2};
    ZeGraphValuePool parameter_pool = {S(ZeGraphValuePool), .bytes = bytes,
        .byte_count = sizeof(bytes)-1, .values = values, .value_count = 7,
        .children = children, .child_count = 6};
    unsigned indices[] = {0, 4, 3, 5};
    ZeGraphParameterValue binding = {S(ZeGraphParameterValue), .name = {0, 1}, .value = indices[shape]};
    const unsigned char plan_bytes[] = "p";
    ZeGraphValuePool plan_pool = {S(ZeGraphValuePool), .bytes = plan_bytes, .byte_count = 1};
    ZeGraphParameter declaration = {S(ZeGraphParameter), .name = {0, 1}, .kinds = shape == 0 ? 16 : 128};
    ZeGraphExpression expressions[] = {
        {S(ZeGraphExpression), .kind = ZE_GRAPH_EXPR_PARAMETER},
        {S(ZeGraphExpression), .kind = ZE_GRAPH_EXPR_SLOT, .value = 9}
    };
    ZeGraphProjection projections[] = {
        {S(ZeGraphProjection), .slot = 9},
        {S(ZeGraphProjection), .slot = 10, .expression = 1}
    };
    uint32_t inputs[] = {0, 1};
    ZeGraphOperator operators[] = {
        {S(ZeGraphOperator), .kind = ZE_GRAPH_OP_UNIT},
        {S(ZeGraphOperator), .kind = ZE_GRAPH_OP_WITH, .inputs = {0, 1}, .projections = {0, 1}},
        {S(ZeGraphOperator), .kind = ZE_GRAPH_OP_PROJECT, .inputs = {1, 1}, .projections = {1, 1}}
    };
    ZeGraphPlan plan = {S(ZeGraphPlan), .root = 2, .pool = &plan_pool,
        .parameters = &declaration, .parameter_count = 1,
        .operators = operators, .operator_count = 3, .inputs = inputs, .input_count = 2,
        .expressions = expressions, .expression_count = 2,
        .projections = projections, .projection_count = 2};
    ZeGraphQueryRequest request = {S(ZeGraphQueryRequest), .plan = &plan,
        .parameters = &binding, .parameter_count = 1, .parameter_pool = &parameter_pool};
    ze_error_code code = ze_graph_query(handle, &request, response);
    char diagnostic[2048] = {0}; size_t written = 0;
    ze_last_error_message(handle.token, diagnostic, sizeof(diagnostic), &written);
    printf("shape=%u status=%d diagnostic=%s\n", shape, (int)code, diagnostic);
    memset(bytes, 0xa5, sizeof(bytes));
    memset(values, 0xa5, sizeof(values));
    memset(children, 0xa5, sizeof(children));
    CHECK(code == ZE_OK);
    return 0;
}
static int string_value(const ZeGraphResponse *r, const ZeGraphValue *v) {
    return v->tag == 4 && v->range.count == 5 &&
        v->range.start + 5 <= r->pool.byte_count &&
        memcmp(r->pool.bytes + v->range.start, "hello", 5) == 0;
}
static const ZeGraphValue *child(const ZeGraphResponse *r, const ZeGraphValue *v, unsigned i) {
    if (v->tag != 7 || i >= v->range.count || v->range.start + i >= r->pool.child_count) return NULL;
    uint32_t index = r->pool.children[v->range.start + i];
    return index < r->pool.value_count ? &r->pool.values[index] : NULL;
}
int main(int argc, char **argv) {
    CHECK(argc == 2);
    ZeGraphOpenRequest open = {S(ZeGraphOpenRequest), .path = {(const uint8_t *)argv[1], strlen(argv[1])},
        .mode = 0, .max_resident_bytes = 256u << 20};
    ZeGraphHandle handle = {0};
    CHECK(ze_graph_open(&open, &handle) == ZE_OK);
    ZeGraphResponse responses[4] = {{S(ZeGraphResponse)}, {S(ZeGraphResponse)}, {S(ZeGraphResponse)}, {S(ZeGraphResponse)}};
    int failures = 0;
    for (unsigned i = 0; i < 4; ++i) failures += query(handle, i, &responses[i]);
    CHECK(ze_graph_close(handle) == ZE_OK);
    if (failures) return 1;
    for (unsigned i = 0; i < 4; ++i) {
        const ZeGraphResponse *r = &responses[i];
        CHECK(r->row_count == 1 && r->column_count == 1 && r->cell_count == 1);
        CHECK(r->cells[0] < r->pool.value_count);
        const ZeGraphValue *v = &r->pool.values[r->cells[0]];
        if (i == 0) CHECK(string_value(r, v));
        if (i == 1) {
            CHECK(v->tag == 7 && v->range.count == 4);
            const ZeGraphValue *a = child(r, v, 0), *b = child(r, v, 1), *c = child(r, v, 2);
            CHECK(a && b && c && string_value(r, a));
            CHECK(b->tag == 2 && b->integer == 3);
            CHECK(c->tag == 7 && c->range.count == 2);
            const ZeGraphValue *d = child(r, v, 3), *e = child(r, c, 0), *f = child(r, c, 1);
            CHECK(d && d->tag == 7 && d->range.count == 0);
            CHECK(e && e->tag == 2 && e->integer == 3);
            CHECK(f && f->tag == 3 && f->floating == 4.5);
        }
        if (i == 2) CHECK(v->tag == 7 && v->range.count == 0);
        if (i == 3) {
            const ZeGraphValue *a = child(r, v, 0), *b = child(r, v, 1);
            CHECK(v->range.count == 2 && a && b);
            CHECK(a->tag == 2 && a->integer == 3 && b->tag == 3 && b->floating == 4.5);
        }
        CHECK(ze_graph_response_free(&responses[i]) == ZE_OK);
    }
    puts("ze311_graph_parameters GREEN: string, nested mixed list, empty list, numeric vector");
    return 0;
}
