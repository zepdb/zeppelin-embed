/* Installed text smoke: no engine sources or model downloads. */
#include "zeppelin_embed.h"
#include <math.h>
#include <stdio.h>
#include <string.h>

#define OK(call) do { \
    ze_error_code code = (call); \
    if (code != ZE_OK) { \
        fprintf(stderr, "%s: %s\n", #call, ze_error_code_name(code)); \
        return 1; \
    } \
} while (0)

int main(int argc, char **argv) {
    if (argc != 3) return 2;
    ZeTextOpenRequest open = {0};
    open.abi_size = sizeof(open);
    open.store.abi_size = sizeof(open.store);
    open.store.path = (const uint8_t *)argv[1];
    open.store.path_len = strlen(argv[1]);
    open.store.commit_tier = 1;
    open.store.reader_drain_timeout_ms = 100;
    open.store.max_resident_bytes = UINT64_MAX;
    open.store.max_temp_bytes = UINT64_MAX;
    open.bundle_path = (const uint8_t *)argv[2];
    open.bundle_path_len = strlen(argv[2]);
    ze_handle handle = 0;
    OK(ze_text_open(&open, &handle));

    const char *text = "hello world";
    ZeTextDocument document = {0};
    document.abi_size = sizeof(document);
    document.doc_id.low = 1;
    document.revision = 1;
    document.text = (const uint8_t *)text;
    document.text_len = strlen(text);
    ZeTextIngestRequest ingest = {0};
    ingest.abi_size = sizeof(ingest);
    ingest.documents = &document;
    ingest.document_count = 1;
    ingest.embed_batch_size = 1;
    ingest.seal_every = 1;
    ingest.channel_capacity = 1;
    ZeMutationReport mutation = {0};
    mutation.abi_size = sizeof(mutation);
    OK(ze_text_ingest(handle, &ingest, &mutation));
    if (mutation.generation == 0) return 1;

    ZeTextQueryRequest query = {0};
    query.abi_size = sizeof(query);
    query.text = (const uint8_t *)text;
    query.text_len = strlen(text);
    query.k = 1;
    query.legs = 0; /* Dense only; a lexical success cannot qualify MLX. */
    ZeTextQueryResult result = {0};
    result.abi_size = sizeof(result);
    OK(ze_text_query(handle, &query, &result));
    if (result.hit_count != 1 || result.hits == NULL ||
        result.hits[0].doc_id.high != 0 || result.hits[0].doc_id.low != 1 ||
        !isfinite(result.hits[0].score) || result.hits[0].has_vector_score != 1 ||
        result.hits[0].has_lexical_score != 0 ||
        !isfinite(result.hits[0].vector_squared_l2)) return 1;
    OK(ze_text_query_result_free(&result));
    OK(ze_close(handle));
    puts("ZE_TEXT_INSTALLED: open, ingest, dense query PASS");
    return 0;
}
