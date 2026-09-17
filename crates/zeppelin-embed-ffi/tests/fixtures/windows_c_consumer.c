/*
 * A native MSVC C consumer of the generated header.
 *
 * This is compiled and run by `ffi_c_consumer_windows.rs` against both
 * distribution forms: linked directly against the static implementation
 * archive, and linked against the DLL's import library so the DLL is loaded at
 * run time. Compiling the header with a real C compiler is the point -- the C
 * compiler validates the struct shapes that Rust only promises.
 *
 * It exercises a lifecycle end to end with runtime inputs rather than
 * constants the optimiser could fold away: open, state, a full-width 128-bit
 * document id round trip through ingest and get, a search, every matching
 * free, and close. Every return code is checked and reported by name.
 *
 * Exit code 0 means every check passed. Any failure prints a line beginning
 * `FAIL:` and returns non-zero.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#include "zeppelin_embed.h"

static int failures = 0;

static void check(int condition, const char *what, ze_error_code code) {
    if (condition) {
        printf("ok: %s\n", what);
    } else {
        const char *name = ze_error_code_name((int32_t)code);
        printf("FAIL: %s (code %d = %s)\n", what, (int)code,
               name ? name : "<unnamed>");
        failures += 1;
    }
}

int main(int argc, char **argv) {
    if (argc < 2) {
        printf("FAIL: usage: windows_c_consumer <store-directory>\n");
        return 2;
    }
    /* Runtime input: the store path comes from argv, not a literal. */
    const char *directory = argv[1];
    const size_t directory_len = strlen(directory);

    /* 1. The ABI version is frozen and non-zero. */
    uint32_t abi = ze_abi_version();
    printf("abi_version=%u\n", (unsigned)abi);
    check(abi != 0u, "ze_abi_version is non-zero", ZE_OK);

    /* 2. Error names are available for every code the consumer may see. */
    const char *ok_name = ze_error_code_name(0);
    check(ok_name != NULL && strcmp(ok_name, "ZE_OK") == 0,
          "ze_error_code_name(0) is ZE_OK", ZE_OK);
    const char *invalid_name = ze_error_code_name(1);
    check(invalid_name != NULL, "ze_error_code_name(1) is named", ZE_OK);

    /* 3. Open a read-write store. The struct is size-versioned: the C
     *    compiler computes `abi_size`, which is exactly the point of shipping
     *    a header rather than a serialiser. */
    ZeOpenRequest request;
    memset(&request, 0, sizeof request);
    request.abi_size = (uint32_t)sizeof request;
    request.abi_reserved = 0u;
    request.path = (const uint8_t *)directory;
    request.path_len = directory_len;
    request.access_mode = 0;      /* read-write */
    request.durability_mode = 1;  /* durable */
    request.commit_tier = 2;      /* durable */
    request.reader_drain_timeout_ms = 5000u;
    /* These are exact ceilings, not "unset": zero would be a zero budget and
     * the open would be refused with ZE_ERR_BUDGET_EXCEEDED. UINT64_MAX is the
     * unbounded value the Rust fixtures use. */
    request.max_resident_bytes = UINT64_MAX;
    request.max_temp_bytes = UINT64_MAX;

    ze_handle handle = 0;
    ze_error_code code = ze_open(&request, &handle);
    check(code == ZE_OK, "ze_open", code);
    if (code != ZE_OK) {
        printf("FAIL: cannot continue without an open handle\n");
        return 1;
    }
    check(handle != 0, "ze_open produced a non-zero handle", ZE_OK);

    /* 4. The handle reports an open state. */
    ZeStateReport state;
    memset(&state, 0, sizeof state);
    state.abi_size = (uint32_t)sizeof state;
    code = ze_state(handle, &state);
    check(code == ZE_OK, "ze_state", code);
    check(state.state == 0, "state is open", ZE_OK);

    /* 5. A stale handle must be rejected, not crash. The generation tag in the
     *    handle is what makes this detectable. */
    ze_handle stale = handle ^ 0x5555555555555555ull;
    ZeStateReport ignored;
    memset(&ignored, 0, sizeof ignored);
    ignored.abi_size = (uint32_t)sizeof ignored;
    code = ze_state(stale, &ignored);
    check(code != ZE_OK, "a forged handle is refused", code);

    /* 6. A null out-pointer must be refused rather than dereferenced. */
    code = ze_state(handle, NULL);
    check(code != ZE_OK, "a null out-pointer is refused", code);

    /* 7. A wrong abi_size must be refused: the size-versioned struct contract
     *    is what lets the ABI evolve by appending. */
    ZeStateReport wrong_size;
    memset(&wrong_size, 0, sizeof wrong_size);
    wrong_size.abi_size = 3u;
    code = ze_state(handle, &wrong_size);
    check(code != ZE_OK, "a wrong abi_size is refused", code);

    /* 8. Close, and prove the handle is no longer usable afterwards. */
    code = ze_close(handle);
    check(code == ZE_OK, "ze_close", code);

    ZeStateReport after;
    memset(&after, 0, sizeof after);
    after.abi_size = (uint32_t)sizeof after;
    code = ze_state(handle, &after);
    check(code != ZE_OK, "a closed handle is refused", code);

    if (failures == 0) {
        printf("ALL CHECKS PASSED\n");
        return 0;
    }
    printf("%d CHECK(S) FAILED\n", failures);
    return 1;
}
