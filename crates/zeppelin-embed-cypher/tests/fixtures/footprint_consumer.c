/* Tooling-only macOS C consumer: compare identical existing core/FFI work with
 * and without an actually called parser. This is a reachable-code lower bound,
 * not the future shipped graph ABI/artifact or its size acceptance. */
#define main ze54_existing_core_main
#include "../../../zeppelin-embed-ffi/tests/fixtures/windows_c_consumer.c"
#undef main
#ifdef ZE54_WITH_FRONTEND
extern size_t ze54_parse_probe(const uint8_t *data, size_t len);
#endif
int main(int argc, char **argv) {
#ifdef ZE54_WITH_FRONTEND
    if (argc != 3) return 2;
    size_t nodes = ze54_parse_probe((const uint8_t *)argv[2], strlen(argv[2]));
    printf("parser_ast_nodes=%zu\n", nodes);
    if (nodes == 0) return 1;
#endif
    return ze54_existing_core_main(argc, argv);
}
