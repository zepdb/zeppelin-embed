#include "zeppelin_embed.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void check(ze_handle handle, ze_error_code status) {
    if (status == ZE_OK) {
        return;
    }
    char message[512] = {0};
    size_t written = 0;
    ze_last_error_message(handle, message, sizeof(message), &written);
    fprintf(stderr, "%s: %.*s\n", ze_error_code_name(status), (int)written, message);
    exit(EXIT_FAILURE);
}

int main(int argc, char **argv) {
    const char *path = argc > 1 ? argv[1] : "zeppelin-example-index";
    const float vectors[5][4] = {
        {0.90f, 0.10f, 0.05f, 0.00f},
        {0.85f, 0.15f, 0.10f, 0.05f},
        {0.10f, 0.90f, 0.05f, 0.00f},
        {0.05f, 0.10f, 0.90f, 0.00f},
        {0.00f, 0.05f, 0.10f, 0.90f},
    };
    ZeIngestDocument documents[5] = {0};
    for (size_t index = 0; index < 5; ++index) {
        documents[index].abi_size = sizeof(ZeIngestDocument);
        documents[index].doc_id.low = index + 1;
        documents[index].revision = 1;
        documents[index].timestamp = (int64_t)(index + 1) * 10;
        documents[index].vector = vectors[index];
        documents[index].vector_len = 4;
    }

    ZeOpenRequest open_request = {0};
    open_request.abi_size = sizeof(ZeOpenRequest);
    open_request.path = (const uint8_t *)path;
    open_request.path_len = strlen(path);
    open_request.reader_drain_timeout_ms = 5000;
    open_request.max_resident_bytes = 512 * 1024 * 1024;
    open_request.max_temp_bytes = 512 * 1024 * 1024;
    ze_handle store = 0;
    check(0, ze_open(&open_request, &store));

    ZeIngestRequest ingest = {0};
    ingest.abi_size = sizeof(ZeIngestRequest);
    ingest.documents = documents;
    ingest.document_count = 5;
    ingest.dimension = 4;
    ZeMutationReport mutation = {.abi_size = sizeof(ZeMutationReport)};
    check(store, ze_ingest(store, &ingest, &mutation));
    printf("ingested 5 vectors at generation %llu\n", (unsigned long long)mutation.generation);

    const float query[] = {0.88f, 0.12f, 0.07f, 0.02f};
    ZeSearchRequest search = {0};
    search.abi_size = sizeof(ZeSearchRequest);
    search.vector = query;
    search.vector_len = 4;
    search.dimension = 4;
    search.k = 3;
    ZeSearchResult result = {.abi_size = sizeof(ZeSearchResult)};
    check(store, ze_search(store, &search, &result));
    for (size_t index = 0; index < result.hit_count; ++index) {
        printf("%zu. document %llu, score %.6f\n", index + 1,
               (unsigned long long)result.hits[index].doc_id.low,
               result.hits[index].score);
    }

    check(store, ze_search_result_free(&result));
    check(store, ze_close(store));
    return EXIT_SUCCESS;
}
