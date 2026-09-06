#include "zeppelin_embed.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void check(ze_handle handle, ze_error_code status) {
    /* Every ABI call returns a typed status. Read the handle's last error for
       the human-readable detail before exiting. */
    if (status == ZE_OK) {
        return;
    }
    char message[512] = {0};
    size_t written = 0;
    ze_last_error_message(handle, message, sizeof(message), &written);
    fprintf(stderr, "%s: %.*s\n", ze_error_code_name(status), (int)written, message);
    exit(EXIT_FAILURE);
}

static ZeAttributeValue u64_attribute(uint32_t id, uint64_t value) {
    ZeAttributeValue attribute = {.attribute_id = id, .value_type = 1};
    attribute.u64_value = value;
    return attribute;
}

static ZeAttributeValue bool_attribute(uint32_t id, int value) {
    ZeAttributeValue attribute = {.attribute_id = id, .value_type = 4};
    attribute.bool_value = value ? 1 : 0;
    return attribute;
}

static ZeAttributeValue string_attribute(uint32_t id, const char *value) {
    ZeAttributeValue attribute = {.attribute_id = id, .value_type = 5};
    attribute.string_value = (const uint8_t *)value;
    attribute.string_len = strlen(value);
    return attribute;
}

static ZeAttributeValue null_attribute(uint32_t id) {
    ZeAttributeValue attribute = {.attribute_id = id, .value_type = 0};
    return attribute;
}

static ZeIngestDocument document(uint64_t id, int64_t timestamp,
                                 const float *vector, size_t vector_len,
                                 const char *text) {
    ZeIngestDocument result = {0};
    result.abi_size = sizeof(ZeIngestDocument);
    result.doc_id.low = id;
    result.revision = 1;
    result.timestamp = timestamp;
    result.vector = vector;
    result.vector_len = vector_len;
    result.text = (const uint8_t *)text;
    result.text_len = strlen(text);
    return result;
}

static ze_handle open_namespace(const char *root, const char *name,
                                const ZeAttributeDefinition *attributes,
                                size_t attribute_count, int has_vector_space,
                                uint32_t dimensions) {
    ZeNamespaceSpec spec = {0};
    spec.abi_size = sizeof(ZeNamespaceSpec);
    spec.attributes = attributes;
    spec.attribute_count = attribute_count;
    spec.has_vector_space = has_vector_space ? 1 : 0;
    spec.dimensions = dimensions;
    spec.normalization = has_vector_space ? 1 : 0;

    ZeNamespaceOpenRequest request = {0};
    request.abi_size = sizeof(ZeNamespaceOpenRequest);
    request.root = (const uint8_t *)root;
    request.root_len = strlen(root);
    request.name = (const uint8_t *)name;
    request.name_len = strlen(name);
    request.open.abi_size = sizeof(ZeOpenRequest);
    request.open.reader_drain_timeout_ms = 5000;
    request.open.max_resident_bytes = 512 * 1024 * 1024;
    request.open.max_temp_bytes = 512 * 1024 * 1024;
    request.spec = &spec;

    ze_handle handle = 0;
    check(0, ze_namespace_open(&request, &handle));
    return handle;
}

int main(int argc, char **argv) {
    /* Each namespace lives at root/name on disk. Pass a database root to keep
       it somewhere else after the example exits. */
    const char *root = argc > 1 ? argv[1] : "zeppelin-record-store-example";
    const ZeAttributeDefinition schema[] = {
        {.attribute_id = 1, .name = (const uint8_t *)"priority", .name_len = 8,
         .attribute_type = 1, .nullable = 0},
        {.attribute_id = 2, .name = (const uint8_t *)"reviewed", .name_len = 8,
         .attribute_type = 4, .nullable = 0},
        {.attribute_id = 3, .name = (const uint8_t *)"category", .name_len = 8,
         .attribute_type = 5, .nullable = 0},
        {.attribute_id = 4, .name = (const uint8_t *)"project", .name_len = 7,
         .attribute_type = 6, .nullable = 1},
    };
    ze_handle notes = open_namespace(root, "notes", schema, 4, 1, 2);

    const float vectors[4][2] = {
        {1.0f, 0.0f},
        {0.0f, 1.0f},
        {0.8f, 0.6f},
        {0.6f, 0.8f},
    };
    ZeAttributeValue attributes[4][4] = {
        {u64_attribute(1, 2), bool_attribute(2, 1),
         string_attribute(3, "work"), string_attribute(4, "zeppelin")},
        {u64_attribute(1, 1), bool_attribute(2, 0),
         string_attribute(3, "personal"), null_attribute(4)},
        {u64_attribute(1, 3), bool_attribute(2, 1),
         string_attribute(3, "work"), string_attribute(4, "zeppelin")},
        {u64_attribute(1, 2), bool_attribute(2, 0),
         string_attribute(3, "personal"), null_attribute(4)},
    };
    const uint64_t ids[] = {101, 102, 103, 104};
    const int64_t timestamps[] = {100, 300, 200, 400};
    const char *texts[] = {
        "Plan the product launch",
        "Buy oat milk",
        "Review search benchmarks",
        "Book dentist appointment",
    };
    ZeUpsertDocument documents[4] = {0};
    for (size_t index = 0; index < 4; ++index) {
        documents[index].abi_size = sizeof(ZeUpsertDocument);
        documents[index].document = document(ids[index], timestamps[index],
                                             vectors[index], 2, texts[index]);
        documents[index].attributes = attributes[index];
        documents[index].attribute_count = 4;
    }
    ZeUpsertRequest upsert = {0};
    upsert.abi_size = sizeof(ZeUpsertRequest);
    upsert.documents = documents;
    upsert.document_count = 4;
    upsert.dimension = 2;
    ZeMutationReport mutation = {.abi_size = sizeof(ZeMutationReport)};
    check(notes, ze_upsert(notes, &upsert, &mutation));
    printf("upserted 4 notes at generation %llu\n",
           (unsigned long long)mutation.generation);

    /* ze_get preserves caller order and returns a slot even for a miss. */
    const ZeDocId requested[] = {{.low = 101}, {.low = 999}};
    ZeGetRequest get = {0};
    get.abi_size = sizeof(ZeGetRequest);
    get.ids = requested;
    get.id_count = 2;
    get.include_text = 1;
    ZeGetResult got = {.abi_size = sizeof(ZeGetResult)};
    check(notes, ze_get(notes, &get, &got));
    if (got.documents[0].has_document) {
        printf("get 101: \"%.*s\"\n", (int)got.documents[0].text_len,
               got.documents[0].text);
    }
    if (!got.documents[1].has_document) {
        printf("get 999: missing (%zu missing)\n", got.missing_count);
    }
    check(notes, ze_get_result_free(&got));

    ZeAttributeValue work = string_attribute(3, "work");
    ZeFilterNode nodes[3] = {0};
    /* Logical children occupy a consecutive range in the flat node array. */
    nodes[0].op = 8;
    nodes[0].children_start = 1;
    nodes[0].children_count = 2;
    nodes[1].op = 1;
    nodes[1].attribute_id = 3;
    nodes[1].values = &work;
    nodes[1].value_count = 1;
    nodes[2].op = 5;
    nodes[2].attribute_id = 1;
    nodes[2].has_lower = 1;
    nodes[2].lower = u64_attribute(1, 2);
    nodes[2].lower_inclusive = 1;
    nodes[2].has_upper = 1;
    nodes[2].upper = u64_attribute(1, 3);
    nodes[2].upper_inclusive = 1;
    ZeFilter filter = {0};
    filter.abi_size = sizeof(ZeFilter);
    filter.nodes = nodes;
    filter.node_count = 3;

    /* A one-row page makes cursor handling visible. */
    ZeScanRequest scan = {0};
    scan.abi_size = sizeof(ZeScanRequest);
    scan.limit = 1;
    scan.order = 1;
    scan.include_text = 1;
    scan.filter = &filter;
    size_t scanned = 0;
    size_t page_number = 1;
    for (;;) {
        ZeScanResult page = {.abi_size = sizeof(ZeScanResult)};
        check(notes, ze_scan(notes, &scan, &page));
        for (size_t index = 0; index < page.document_count; ++index) {
            const ZeStoredDocument *note = &page.documents[index];
            printf("scan page %zu: note %llu at %lld: \"%.*s\"\n",
                   page_number, (unsigned long long)note->doc_id.low,
                   (long long)note->timestamp, (int)note->text_len, note->text);
        }
        scanned += page.document_count;
        scan.cursor_generation = page.generation;
        memcpy(scan.cursor_segment_id, page.next_segment_id,
               sizeof(scan.cursor_segment_id));
        scan.cursor_next_row = page.next_row;
        scan.cursor_phase = page.next_phase;
        int has_more = page.has_more;
        check(notes, ze_scan_result_free(&page));
        if (!has_more) {
            break;
        }
        ++page_number;
    }

    ZeCountRequest count_request = {0};
    count_request.abi_size = sizeof(ZeCountRequest);
    count_request.filter = &filter;
    ZeCountResult count = {.abi_size = sizeof(ZeCountResult)};
    check(notes, ze_count(notes, &count_request, &count));
    printf("count: %llu matching notes (scan found %zu)\n",
           (unsigned long long)count.count, scanned);

    const float query[] = {1.0f, 0.0f};
    ZeSearchFilteredRequest filtered = {0};
    filtered.abi_size = sizeof(ZeSearchFilteredRequest);
    filtered.search.abi_size = sizeof(ZeSearchRequest);
    filtered.search.vector = query;
    filtered.search.vector_len = 2;
    filtered.search.dimension = 2;
    filtered.search.k = 2;
    filtered.search.has_tier = 1;
    filtered.search.tier = 1;
    filtered.filter = &filter;
    ZeSearchResult matches = {.abi_size = sizeof(ZeSearchResult)};
    check(notes, ze_search_filtered(notes, &filtered, &matches));
    for (size_t index = 0; index < matches.hit_count; ++index) {
        printf("search %zu: note %llu, score %.3f\n", index + 1,
               (unsigned long long)matches.hits[index].doc_id.low,
               matches.hits[index].score);
    }
    check(notes, ze_search_result_free(&matches));
    check(notes, ze_close(notes));

    /* Omitting a vector space creates a plain record store. Vector search on
       this namespace is rejected with ZE_ERR_NO_VECTOR_SPACE. */
    ze_handle inbox = open_namespace(root, "inbox", NULL, 0, 0, 0);
    ZeUpsertDocument record = {.abi_size = sizeof(ZeUpsertDocument)};
    record.document = document(201, 500, NULL, 0, "Call Alice");
    ZeUpsertRequest record_upsert = {0};
    record_upsert.abi_size = sizeof(ZeUpsertRequest);
    record_upsert.documents = &record;
    record_upsert.document_count = 1;
    check(inbox, ze_upsert(inbox, &record_upsert, &mutation));

    ZeScanRequest record_scan = {0};
    record_scan.abi_size = sizeof(ZeScanRequest);
    record_scan.limit = 10;
    record_scan.order = 1;
    record_scan.include_text = 1;
    ZeScanResult records = {.abi_size = sizeof(ZeScanResult)};
    check(inbox, ze_scan(inbox, &record_scan, &records));
    printf("record-only scan: note %llu: \"%.*s\"\n",
           (unsigned long long)records.documents[0].doc_id.low,
           (int)records.documents[0].text_len, records.documents[0].text);
    check(inbox, ze_scan_result_free(&records));
    check(inbox, ze_close(inbox));
    return EXIT_SUCCESS;
}
