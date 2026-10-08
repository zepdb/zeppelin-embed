/* Genuine installed consumer. Build only with installed headers and archive/dylib.
 * graph_profile_cases.h is generated from the shared ZE-74 manifest. */
#include "zeppelin_embed.h"
#include "zeppelin_graph_contracts.h"
#include <assert.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include "graph_profile_cases.h"

static ze_error_code fixture_store_open(const ZeGraphOpenRequest *request, ze_handle *out) {
    if (request->mode == 0) return ze_store_create_with_relationship_types(request, NULL, 0, out);
    ZeOpenRequest open = {0}; open.abi_size = sizeof(open);
    open.path = request->path.data; open.path_len = request->path.count;
    open.access_mode = request->mode == 2; open.durability_mode = 1; open.commit_tier = 2;
    open.reader_drain_timeout_ms = request->reader_drain_timeout_ms;
    open.max_resident_bytes = request->max_resident_bytes; open.max_temp_bytes = UINT64_MAX;
    if (!request->document_tower) return ze_open(&open, out);
    ZeEpochRequest epoch = {0}; epoch.abi_size = sizeof(epoch);
    epoch.embedding.document = *request->document_tower; epoch.embedding.query = *request->document_tower;
    return ze_open_with_epoch(&open, &epoch, out);
}

#define SIZED(T) ((T){.abi_size = sizeof(T)})
static void string(const uint8_t *s, size_t n) {
    putchar('"');
    for (size_t i=0;i<n;i++) {
        unsigned char c=s[i];
        if(c=='"'||c=='\\') {putchar('\\');putchar(c);}
        else if(c<32) printf("\\u%04x",c);
        else putchar(c);
    }
    putchar('"');
}
static void text(const ZeGraphValuePool *p, ZeGraphRange r) {
    assert((size_t)r.start+r.count<=p->byte_count); string(p->bytes+r.start,r.count);
}
static void value(const ZeGraphValuePool *, uint32_t, unsigned);
static void properties(const ZeGraphValuePool *p, ZeGraphRange range, unsigned depth) {
    putchar('{');
    for(uint32_t i=0;i<range.count;i++) {if(i)putchar(',');assert(range.start+i<p->property_count);
        ZeGraphProperty property=p->properties[range.start+i];text(p,property.name);putchar(':');value(p,property.value,depth+1);}
    putchar('}');
}
static void value(const ZeGraphValuePool *p,uint32_t index,unsigned depth) {
    assert(depth<=16 && index<p->value_count);const ZeGraphValue *v=&p->values[index];
    printf("{\"type\":%u,\"value\":",v->tag);
    switch(v->tag) {
    case 0:printf("null");break;
    case 1:printf("%s",v->boolean?"true":"false");break;
    case 2:printf("%lld",(long long)v->integer);break;
    case 3:printf("%.17g",v->floating);break;
    case 4:text(p,v->range);break;
    case 7:putchar('[');for(uint32_t i=0;i<v->range.count;i++){if(i)putchar(',');assert(v->range.start+i<p->child_count);value(p,p->children[v->range.start+i],depth+1);}putchar(']');break;
    case 5:{assert(v->entity_index<p->node_count);ZeGraphNode n=p->nodes[v->entity_index];printf("{\"labels\":[");for(uint32_t i=0;i<n.labels.count;i++){if(i)putchar(',');assert(n.labels.start+i<p->name_count);text(p,p->names[n.labels.start+i]);}printf("],\"properties\":");properties(p,n.properties,depth);putchar('}');break;}
    case 6:{assert(v->entity_index<p->relationship_count);ZeGraphRelationship r=p->relationships[v->entity_index];printf("{\"kind\":");text(p,r.relationship_type);printf(",\"properties\":");properties(p,r.properties,depth);putchar('}');break;}
    default:assert(!"unknown graph value tag");
    }putchar('}');
}
static ZeGraphResponse query(ze_handle h,const char *q,int32_t *status) {
    ZeGraphCypherRequest request=SIZED(ZeGraphCypherRequest);request.query=(ZeGraphBytes){(const uint8_t*)q,strlen(q)};
    ZeGraphResponse r=SIZED(ZeGraphResponse);r.pool.abi_size=sizeof(r.pool);*status=ze_store_cypher(h,&request,&r);return r;
}
static void rows(const ZeGraphResponse *r) {
    assert(r->cell_count==r->row_count*r->column_count);putchar('[');
    for(size_t row=0;row<r->row_count;row++){if(row)putchar(',');putchar('[');
        for(size_t col=0;col<r->column_count;col++){if(col)putchar(',');value(&r->pool,r->cells[row*r->column_count+col],0);}putchar(']');}putchar(']');
}
static void snapshot(ze_handle h) {
    printf("[");for(unsigned rel=0;rel<2;rel++){int32_t status;ZeGraphResponse r=query(h,rel?"MATCH ()-[r]->() RETURN ze.relationship_id(r), r":"MATCH (n) RETURN ze.node_id(n), n",&status);assert(status==ZE_OK);assert(r.pool.byte_count<=24UL<<20);if(rel)putchar(',');rows(&r);assert(ze_graph_response_free(&r)==ZE_OK);}putchar(']');
}
int main(int argc,char **argv) {
    assert(argc==2);
    for(size_t c=0;c<sizeof(ze74_cases)/sizeof(ze74_cases[0]);c++) {
        const Ze74Case *fixture=&ze74_cases[c];
        for(unsigned structured=0;structured<=fixture->structured;structured++) {
        char path[4096];assert(snprintf(path,sizeof(path),"%s/case-%zu-%u",argv[1],c,structured)<(int)sizeof(path));
        ZeGraphOpenRequest open=SIZED(ZeGraphOpenRequest);open.path=(ZeGraphBytes){(const uint8_t*)path,strlen(path)};open.max_resident_bytes=256ULL<<20;open.reader_drain_timeout_ms=250;
        ze_handle h={0};assert(fixture_store_open(&open,&h)==ZE_OK);
        for(size_t i=0;i<fixture->setup_count;i++){int32_t status;ZeGraphResponse r=query(h,fixture->setup[i],&status);assert(status==ZE_OK);assert(ze_graph_response_free(&r)==ZE_OK);}
        printf("{\"case\":");string((const uint8_t*)fixture->id,strlen(fixture->id));printf(",\"path\":\"%s\",\"before\":",structured?"c-structured":"c-cypher");snapshot(h);
        int32_t status;ZeGraphResponse r=SIZED(ZeGraphResponse);r.pool.abi_size=sizeof(r.pool);
        if(structured)status=ze74_structured(c,h,&r);else r=query(h,fixture->query,&status);printf(",\"status\":%d,\"disposition\":%u,\"admitted_generation\":%llu,\"changed_generation\":%llu,\"after\":",status,r.disposition,(unsigned long long)r.admitted_generation,(unsigned long long)r.changed_generation);snapshot(h);
        assert(ze_close(h)==ZE_OK);open.mode=1;assert(fixture_store_open(&open,&h)==ZE_OK);printf(",\"reopened_snapshot\":");snapshot(h);assert(ze_close(h)==ZE_OK);
        printf(",\"rows\":");rows(&r);printf(",\"columns\":[");for(size_t i=0;i<r.column_count;i++){if(i)putchar(',');text(&r.pool,r.columns[i].name);}printf("],\"column_kinds\":[");for(size_t i=0;i<r.column_count;i++){if(i)putchar(',');printf("%u",r.columns[i].kinds);}printf("]}");assert(ze_graph_response_free(&r)==ZE_OK);putchar('\n');
        }
    }return 0;
}
