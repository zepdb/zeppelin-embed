/* ZE-77 public C worker. Bounded tooling records; no engine serializer. */
#define _POSIX_C_SOURCE 200809L
#include "zeppelin_embed.h"
#include "zeppelin_graph_contracts.h"
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <pthread.h>
#define MAX 4096
#define SIZED(T) ((T){.abi_size=sizeof(T)})
typedef struct {
    ZeGraphOperator operators[MAX]; size_t no;
    ZeGraphExpression expressions[MAX]; size_t ne;
    ZeGraphValue values[MAX]; size_t nv;
    ZeGraphRange names[MAX]; size_t nn;
    ZeGraphProjection projections[MAX]; size_t np;
    ZeGraphSortKey sort[MAX]; size_t nt;
    ZeGraphSearch searches[8]; ZeGraphSearchOptions options[8]; size_t ns;
    uint32_t inputs[MAX*2], children[MAX*2], eager[8]; size_t ni,nc,ng;
    uint8_t bytes[65536]; size_t nb;
    ZeGraphNode nodes[64]; size_t nd;
    ZeGraphRelationship relationships[128]; size_t nr;
    ZeGraphProperty properties[512]; size_t nq;
    ZeGraphBatchItem items[256]; size_t na;
    uint32_t value_children[512]; size_t nvc;
    float vectors[20*768]; size_t nf;
    ZeGraphPlan plan; ZeGraphValuePool pool;
} Job;
static uint64_t now(void) { struct timespec t; if(clock_gettime(CLOCK_MONOTONIC,&t)) exit(2); return (uint64_t)t.tv_sec*1000000000ULL+(uint64_t)t.tv_nsec; }
static void die(const char *message){fprintf(stderr,"ZE-77 C: %s\n",message);exit(2);}
static uint64_t word(FILE *f){unsigned long long v;if(fscanf(f," %llu",&v)!=1)die("truncated tooling job");return (uint64_t)v;}
static void bound(size_t n,size_t max){if(n>=max)die("tooling job exceeds declared bounds");}
static Job *load(const char *path){
    FILE *f=fopen(path,"r");if(!f)die("cannot open tooling job");char magic[16];if(fscanf(f,"%15s",magic)!=1||strcmp(magic,"ZE77JOB1"))die("wrong job version");
    Job *j=calloc(1,sizeof(*j));if(!j)die("job allocation failed");j->plan=SIZED(ZeGraphPlan);j->pool=SIZED(ZeGraphValuePool);
    char tag;while(fscanf(f," %c",&tag)==1){
        switch((unsigned char)tag){
        case 'R': j->plan.root=(uint32_t)word(f);break;
        case 'B': {char *hex=malloc(131073);if(!hex)die("hex allocation");if(fscanf(f," %131072s",hex)!=1)die("missing job bytes");size_t n=strlen(hex);if(n%2||n/2>sizeof(j->bytes))die("invalid byte pool");for(size_t i=0;i<n/2;i++){unsigned v;if(sscanf(hex+2*i,"%2x",&v)!=1)die("invalid byte hex");j->bytes[i]=(uint8_t)v;}j->nb=n/2;free(hex);break;}
        case 'V': {bound(j->nv,MAX);ZeGraphValue *v=&j->values[j->nv++];*v=SIZED(ZeGraphValue);v->tag=(uint32_t)word(f);long long integer;if(fscanf(f," %lld",&integer)!=1)die("integer literal");v->integer=integer;uint64_t bits=word(f);memcpy(&v->floating,&bits,8);v->range.start=(uint32_t)word(f);v->range.count=(uint32_t)word(f);break;}
        case 'F': {bound(j->nf,20*768);uint32_t bits=(uint32_t)word(f);memcpy(&j->vectors[j->nf++],&bits,4);break;}
        case 'C': bound(j->nvc,512);j->value_children[j->nvc++]=(uint32_t)word(f);break;
        case 'W': {bound(j->nv,MAX);ZeGraphValue *v=&j->values[j->nv++];*v=SIZED(ZeGraphValue);v->tag=(uint32_t)word(f);v->list_kind=(uint32_t)word(f);v->boolean=(uint32_t)word(f);long long integer;if(fscanf(f," %lld",&integer)!=1)die("property integer");v->integer=integer;uint64_t bits=word(f);memcpy(&v->floating,&bits,8);v->range.start=(uint32_t)word(f);v->range.count=(uint32_t)word(f);break;}
        case 'Q': {bound(j->nq,512);ZeGraphProperty *p=&j->properties[j->nq++];*p=SIZED(ZeGraphProperty);p->name.start=(uint32_t)word(f);p->name.count=(uint32_t)word(f);p->value=(uint32_t)word(f);break;}
        case 'D': {bound(j->nd,64);ZeGraphNode *n=&j->nodes[j->nd++];*n=SIZED(ZeGraphNode);n->has_text=(uint32_t)word(f);n->has_vector=(uint32_t)word(f);n->properties.start=(uint32_t)word(f);n->properties.count=(uint32_t)word(f);n->text.start=(uint32_t)word(f);n->text.count=(uint32_t)word(f);n->vector.start=(uint32_t)word(f);n->vector.count=(uint32_t)word(f);n->labels.start=(uint32_t)word(f);n->labels.count=(uint32_t)word(f);break;}
        case 'L': {bound(j->nr,128);ZeGraphRelationship *r=&j->relationships[j->nr++];*r=SIZED(ZeGraphRelationship);r->properties.start=(uint32_t)word(f);r->properties.count=(uint32_t)word(f);r->relationship_type.start=(uint32_t)word(f);r->relationship_type.count=(uint32_t)word(f);break;}
        case 'A': {bound(j->na,256);ZeGraphBatchItem *a=&j->items[j->na++];*a=SIZED(ZeGraphBatchItem);a->entity_kind=(uint32_t)word(f);a->has_image=1;a->revision=1;a->namespace_name.start=(uint32_t)word(f);a->namespace_name.count=(uint32_t)word(f);a->key.start=(uint32_t)word(f);a->key.count=(uint32_t)word(f);a->image=(uint32_t)word(f);ZeGraphEndpoint *ends[2]={&a->source,&a->target};for(size_t e=0;e<2;e++){*ends[e]=SIZED(ZeGraphEndpoint);ends[e]->kind=(uint32_t)word(f);ends[e]->local_item=(uint32_t)word(f);ends[e]->node.high=word(f);ends[e]->node.low=word(f);}break;}
        case 'N': {bound(j->nn,MAX);uint32_t start=(uint32_t)word(f),count=(uint32_t)word(f);j->names[j->nn++]=(ZeGraphRange){start,count};break;}
        case 'E': {bound(j->ne,MAX);ZeGraphExpression *e=&j->expressions[j->ne++];*e=SIZED(ZeGraphExpression);e->kind=(uint32_t)word(f);e->operation=(uint32_t)word(f);e->left=(uint32_t)word(f);e->right=(uint32_t)word(f);e->has_operand=(uint32_t)word(f);e->distinct=(uint32_t)word(f);e->value=(uint32_t)word(f);e->name.start=(uint32_t)word(f);e->name.count=(uint32_t)word(f);e->children.start=(uint32_t)word(f);e->children.count=(uint32_t)word(f);break;}
        case 'P': {bound(j->np,MAX);ZeGraphProjection *p=&j->projections[j->np++];*p=SIZED(ZeGraphProjection);p->slot=(uint32_t)word(f);p->expression=(uint32_t)word(f);break;}
        case 'T': {bound(j->nt,MAX);ZeGraphSortKey *s=&j->sort[j->nt++];*s=SIZED(ZeGraphSortKey);s->expression=(uint32_t)word(f);s->descending=(uint32_t)word(f);break;}
        case 'I': bound(j->ni,MAX*2);j->inputs[j->ni++]=(uint32_t)word(f);break;
        case 'H': bound(j->nc,MAX*2);j->children[j->nc++]=(uint32_t)word(f);break;
        case 'G': bound(j->ng,8);j->eager[j->ng++]=(uint32_t)word(f);break;
        case 'S': {bound(j->ns,8);size_t i=j->ns++;ZeGraphSearch *s=&j->searches[i];*s=SIZED(ZeGraphSearch);s->kind=(uint32_t)word(f);s->call_id=(uint32_t)word(f);s->vector.present=(uint32_t)word(f);s->vector.index=(uint32_t)word(f);s->text.present=(uint32_t)word(f);s->text.index=(uint32_t)word(f);s->k=(uint32_t)word(f);s->has_tier=(uint32_t)word(f);s->tier=(uint32_t)word(f);s->eligible_set.present=(uint32_t)word(f);s->eligible_set.index=(uint32_t)word(f);s->node_slot=(uint32_t)word(f);s->score_slot=(uint32_t)word(f);s->vector_distance_slot.present=(uint32_t)word(f);s->vector_distance_slot.index=(uint32_t)word(f);s->lexical_score_slot.present=(uint32_t)word(f);s->lexical_score_slot.index=(uint32_t)word(f);j->options[i]=SIZED(ZeGraphSearchOptions);j->options[i].has_alpha=(uint32_t)word(f);if(fscanf(f," %lf",&j->options[i].alpha)!=1)die("alpha");s->options=&j->options[i];break;}
        case 'O': {bound(j->no,MAX);ZeGraphOperator *o=&j->operators[j->no++];*o=SIZED(ZeGraphOperator);uint64_t a[25];for(size_t i=0;i<25;i++)a[i]=word(f);o->kind=(uint32_t)a[0];o->source_slot=(uint32_t)a[1];o->node_slot=(uint32_t)a[2];o->relationship_slot=(uint32_t)a[3];o->direction=(uint32_t)a[4];o->pattern=(uint32_t)a[5];o->relationship_types=(ZeGraphRange){(uint32_t)a[6],(uint32_t)a[7]};o->path_min=(uint32_t)a[8];o->path_max=(uint32_t)a[9];o->set_slot=(uint32_t)a[10];o->predicate.index=(uint32_t)a[11];o->predicate.present=(uint32_t)a[12];o->sort_keys=(ZeGraphRange){(uint32_t)a[13],(uint32_t)a[14]};o->offset=a[15];o->limit=a[16];o->has_limit=(uint32_t)a[17];o->projections=(ZeGraphRange){(uint32_t)a[18],(uint32_t)a[19]};o->search=(uint32_t)a[20];o->node_id=(ZeNodeId){a[21],a[22]};o->inputs=(ZeGraphRange){(uint32_t)a[23],(uint32_t)a[24]};break;}
        default:die("unknown tooling record");
        }
    }fclose(f);
    j->pool.bytes=j->bytes;j->pool.byte_count=j->nb;j->pool.values=j->values;j->pool.value_count=j->nv;j->pool.names=j->names;j->pool.name_count=j->nn;
    j->pool.nodes=j->nodes;j->pool.node_count=j->nd;j->pool.relationships=j->relationships;j->pool.relationship_count=j->nr;j->pool.properties=j->properties;j->pool.property_count=j->nq;j->pool.children=j->value_children;j->pool.child_count=j->nvc;j->pool.vectors=j->vectors;j->pool.vector_count=j->nf;
    j->plan.operators=j->operators;j->plan.operator_count=j->no;j->plan.expressions=j->expressions;j->plan.expression_count=j->ne;j->plan.inputs=j->inputs;j->plan.input_count=j->ni;j->plan.expression_children=j->children;j->plan.expression_child_count=j->nc;j->plan.projections=j->projections;j->plan.projection_count=j->np;j->plan.sort_keys=j->sort;j->plan.sort_key_count=j->nt;j->plan.searches=j->searches;j->plan.search_count=j->ns;j->plan.eager_searches=j->eager;j->plan.eager_search_count=j->ng;j->plan.pool=&j->pool;return j;
}
static void string(const uint8_t *p,size_t n){putchar('"');for(size_t i=0;i<n;i++){unsigned b=p[i];if(b=='"'||b=='\\'){putchar('\\');putchar((int)b);}else if(b<32)printf("\\u%04x",b);else putchar((int)b);}putchar('"');}
static void id(uint64_t high,uint64_t low){unsigned __int128 n=((unsigned __int128)high<<64)|low;char text[40];size_t i=sizeof(text);do{text[--i]=(char)('0'+n%10);n/=10;}while(n);string((const uint8_t *)text+i,sizeof(text)-i);}
static void cell(const ZeGraphResponse *r,uint32_t index,unsigned depth){if(index>=r->pool.value_count||depth>64)die("invalid completed value");const ZeGraphValue *v=&r->pool.values[index];switch(v->tag){
case 0:printf("{\"null\":true}");break;case 1:printf("{\"bool\":%s}",v->boolean?"true":"false");break;case 2:printf("{\"i64\":%" PRId64 "}",v->integer);break;
case 3:{uint64_t b;memcpy(&b,&v->floating,8);printf("{\"f64_bits\":\"%016" PRIx64 "\"}",b);break;}
case 4:if((uint64_t)v->range.start+v->range.count>r->pool.byte_count)die("string range");printf("{\"string\":");string(r->pool.bytes+v->range.start,v->range.count);putchar('}');break;
case 5:if(v->entity_index>=r->pool.node_count)die("node index");printf("{\"node\":");id(r->pool.nodes[v->entity_index].id.high,r->pool.nodes[v->entity_index].id.low);putchar('}');break;
case 6:if(v->entity_index>=r->pool.relationship_count)die("rel index");printf("{\"relationship\":");id(r->pool.relationships[v->entity_index].id.high,r->pool.relationships[v->entity_index].id.low);putchar('}');break;
case 7:if((uint64_t)v->range.start+v->range.count>r->pool.child_count)die("child range");printf("{\"list\":[");for(uint32_t i=0;i<v->range.count;i++){if(i)putchar(',');cell(r,r->pool.children[v->range.start+i],depth+1);}printf("]}");break;default:die("unknown result tag");}}
static ZeGraphHandle open_store(const char *path){ZeGraphOpenRequest o=SIZED(ZeGraphOpenRequest);o.path=(ZeGraphBytes){(const uint8_t *)path,strlen(path)};o.mode=1;o.reader_drain_timeout_ms=5000;o.max_resident_bytes=256ULL<<20;const uint8_t hash[]={0x73};ZeEmbeddingTower t={.model_id=(const uint8_t *)"ze73-fixture",.model_id_len=12,.model_version=(const uint8_t *)"1",.model_version_len=1,.weights_digest=hash,.weights_digest_len=1,.dims=768,.max_tokens=512,.runtime=3,.compute_units=1};o.document_tower=&t;ZeGraphHandle h={0};int status=ze_graph_open(&o,&h);if(status){fprintf(stderr,"ZE-77 C open status=%d\n",status);exit(1);}return h;}
static char **schedule(const char *path,size_t *count){
 char **paths=calloc(10000,sizeof(*paths));if(!paths)die("schedule allocation");*count=0;FILE *f=fopen(path,"r");if(!f)die("schedule input");char *line=NULL;size_t capacity=0;ssize_t length;
 while((length=getline(&line,&capacity,f))>0){bound(*count,10000);if(line[length-1]=='\n')line[--length]=0;if(length<=0||length>4096)die("schedule path");paths[(*count)++]=strdup(line);}free(line);fclose(f);if(!*count)die("empty schedule");return paths;
}
static int run_loop(ZeGraphHandle h,int structured,int batch,char **paths,size_t path_count,unsigned long warmups,unsigned long samples,int participant,pthread_mutex_t *output){
 Job *j=NULL;char source[65537];size_t source_len=0;int failed=0;
    for(unsigned long i=0;i<warmups+samples;i++){
        const char *job=paths[(i+(unsigned long)(participant==4?0:participant)*17)%path_count];
        if(structured||batch){j=load(job);}else{FILE *f=fopen(job,"rb");if(!f)die("source input");source_len=fread(source,1,65537,f);fclose(f);if(source_len>65536)die("source exceeds compiler limit");}
        ZeGraphResponse r=SIZED(ZeGraphResponse);r.pool.abi_size=sizeof(r.pool);int status;uint64_t start=now();
        if(batch){ZeGraphBatchRequest q=SIZED(ZeGraphBatchRequest);q.items=j->items;q.item_count=j->na;q.pool=&j->pool;status=ze_graph_apply(h,&q,&r);}else if(structured){ZeGraphQueryRequest q=SIZED(ZeGraphQueryRequest);q.plan=&j->plan;status=ze_graph_query(h,&q,&r);}else{ZeGraphCypherRequest q=SIZED(ZeGraphCypherRequest);q.query=(ZeGraphBytes){(const uint8_t *)source,source_len};status=ze_graph_cypher(h,&q,&r);}uint64_t elapsed=now()-start;
        {if(output)pthread_mutex_lock(output);printf("{\"participant\":%d,\"warmup\":%s,\"schedule_index\":%lu,\"sample\":%lu,\"elapsed_ns\":%" PRIu64 ",\"generation\":%" PRIu64 ",\"status\":%d,\"rows\":[",participant,i<warmups?"true":"false",(i+(unsigned long)(participant==4?0:participant)*17)%path_count,i>=warmups?i-warmups:0,elapsed,r.admitted_generation,status);for(size_t row=0;row<r.row_count;row++){if(row)putchar(',');putchar('[');for(size_t col=0;col<r.column_count;col++){if(col)putchar(',');cell(&r,r.cells[row*r.column_count+col],0);}putchar(']');}printf("],\"receipts\":[");for(size_t k=0;k<r.receipt_count;k++){const ZeGraphReceipt *receipt=&r.receipts[k];if(k)putchar(',');printf("{\"item\":%u,\"kind\":\"%s\",\"revision\":%" PRIu64 ",\"generation\":%" PRIu64 ",\"id\":",receipt->item,receipt->entity_kind==0?"node":"relationship",receipt->revision,receipt->generation);if(receipt->entity_kind==0)id(receipt->node.high,receipt->node.low);else id(receipt->relationship.high,receipt->relationship.low);putchar('}');}printf("],\"work_raw\":[");for(size_t w=0;w<r.work_count;w++){if(w)putchar(',');printf("{\"kind\":%u,\"value\":%" PRIu64 "}",r.work[w].kind,r.work[w].value);}printf("],\"global_work\":{\"start\":%u,\"count\":%u},\"missing_input\":\"ZE-76 complete resource/work reporting\"",r.global_work.start,r.global_work.count);}
        uint64_t dispose=now();int freed=ze_graph_response_free(&r);dispose=now()-dispose;printf(",\"disposal_ns\":%" PRIu64 ",\"free_status\":%d}\n",dispose,freed);if(output)pthread_mutex_unlock(output);free(j);j=NULL;if(status||freed){failed=1;break;}
    }
 free(j);return failed;
}
typedef struct {pthread_mutex_t mutex;pthread_cond_t ready;unsigned arrived;} Barrier;
static void start_barrier(Barrier *b){pthread_mutex_lock(&b->mutex);if(++b->arrived==5)pthread_cond_broadcast(&b->ready);while(b->arrived<5)pthread_cond_wait(&b->ready,&b->mutex);pthread_mutex_unlock(&b->mutex);}
typedef struct {ZeGraphHandle handle;Barrier *barrier;pthread_mutex_t *output;char **paths;size_t count;int participant;int failed;} Participant;
static void *participate(void *arg){Participant *p=arg;start_barrier(p->barrier);p->failed=run_loop(p->handle,p->participant!=4,p->participant==4,p->paths,p->count,0,p->participant==4?200:1000,p->participant,p->output);return NULL;}
static int mixed(const char *store,const char *read_list,const char *write_list){
 size_t nr,nw;char **reads=schedule(read_list,&nr),**writes=schedule(write_list,&nw);if(nr!=100||nw!=200)die("mixed load requires 100 cases and 200 distinct meeting batches");
 ZeGraphHandle handle=open_store(store);Barrier barrier={.mutex=PTHREAD_MUTEX_INITIALIZER,.ready=PTHREAD_COND_INITIALIZER,.arrived=0};pthread_mutex_t output=PTHREAD_MUTEX_INITIALIZER;pthread_t threads[5];Participant participants[5];
 for(int i=0;i<5;i++){participants[i]=(Participant){handle,&barrier,&output,i==4?writes:reads,i==4?nw:nr,i,0};if(pthread_create(&threads[i],NULL,participate,&participants[i]))die("mixed startup failed");}
 int failed=0;for(int i=0;i<5;i++){if(pthread_join(threads[i],NULL))die("mixed join failed");failed|=participants[i].failed;}if(ze_graph_close(handle))failed=1;
 for(size_t i=0;i<nr;i++)free(reads[i]);for(size_t i=0;i<nw;i++)free(writes[i]);free(reads);free(writes);pthread_cond_destroy(&barrier.ready);pthread_mutex_destroy(&barrier.mutex);pthread_mutex_destroy(&output);return failed;
}
int main(int argc,char **argv){
    if(argc==5 && strcmp(argv[1],"mixed")==0)return mixed(argv[2],argv[3],argv[4]);
    if(argc!=6){fprintf(stderr,"usage: graph-workload-c STORE structured|cypher JOB WARMUPS SAMPLES\n");return 2;}
    char *end=NULL;unsigned long warmups=strtoul(argv[4],&end,10);if(!end||*end||warmups>1000)die("invalid warmups");unsigned long samples=strtoul(argv[5],&end,10);if(!end||*end||samples==0||samples>10000)die("invalid samples");
    int batch=strcmp(argv[2],"batch")==0;int structured=strcmp(argv[2],"structured")==0;if(!structured&&!batch&&strcmp(argv[2],"cypher"))die("unknown frontend");char **paths=calloc(10000,sizeof(*paths));if(!paths)die("schedule allocation");size_t path_count=0;
    if(argv[3][0]=='@'){FILE *f=fopen(argv[3]+1,"r");if(!f)die("schedule input");char *line=NULL;size_t capacity=0;ssize_t length;while((length=getline(&line,&capacity,f))>0){bound(path_count,10000);if(line[length-1]=='\n')line[--length]=0;if(length<=0||length>4096)die("schedule path");paths[path_count++]=strdup(line);}free(line);fclose(f);}else{paths[path_count++]=strdup(argv[3]);}if(!path_count)die("empty schedule");
    ZeGraphHandle h=open_store(argv[1]);int failed=0;
    failed=run_loop(h,structured,batch,paths,path_count,warmups,samples,0,NULL);
    for(size_t i=0;i<path_count;i++)free(paths[i]);free(paths);if(ze_graph_close(h))failed=1;return failed;
}
