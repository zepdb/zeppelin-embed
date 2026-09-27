#!/usr/bin/env bash

# Resolve the Cargo target that controls whether native graph qualification is
# available. A command-line --target takes precedence over CARGO_BUILD_TARGET,
# matching Cargo's own selection order. Without either override, use the
# selected rustc toolchain's host triple as Cargo's default target.
graph_effective_target() {
    local option
    while (( $# > 0 )); do
        option="$1"
        case "$option" in
            --target)
                if (( $# > 1 )); then
                    printf '%s\n' "$2"
                    return
                fi
                ;;
            --target=*)
                printf '%s\n' "${option#--target=}"
                return
                ;;
        esac
        shift
    done

    if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
        printf '%s\n' "$CARGO_BUILD_TARGET"
        return
    fi

    local rustc_host
    rustc_host="$(rustc -vV | sed -n 's/^host: //p')"
    if [[ -z "$rustc_host" ]]; then
        echo 'unable to resolve the selected rustc host target' >&2
        return 2
    fi
    printf '%s\n' "$rustc_host"
}

graph_target_supports_native_graph() {
    case "$(uname -s):$1" in
        Darwin:aarch64-apple-darwin|Darwin:x86_64-apple-darwin|\
        MINGW*:x86_64-pc-windows-msvc|MSYS*:x86_64-pc-windows-msvc|\
        CYGWIN*:x86_64-pc-windows-msvc|Windows_NT:x86_64-pc-windows-msvc)
            return 0 ;;
        *) return 1 ;;
    esac
}

graph_host_is_darwin() {
    [[ "$(uname -s)" == "Darwin" ]]
}
