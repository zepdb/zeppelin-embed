#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUDGET_KB="${ZE_SIZE_BUDGET_KB:-5120}"

if [[ ! "$BUDGET_KB" =~ ^[0-9]+$ ]]; then
    echo "error: ZE_SIZE_BUDGET_KB must be a non-negative integer, got '$BUDGET_KB'" >&2
    exit 2
fi

cd "$PROJECT_ROOT"
if [[ "$(uname -s)" == "Darwin" ]]; then
    cargo build --release -p zeppelin-embed -p zeppelin-embed-ffi -p zeppelin-embed-text
else
    cargo build --release -p zeppelin-embed -p zeppelin-embed-ffi
fi

TARGET_ROOT="${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}"
MEASURE_DIR="$TARGET_ROOT/size-budget"

if ! command -v strip >/dev/null 2>&1; then
    echo "error: platform 'strip' tool is required for the size gate" >&2
    exit 2
fi
if ! command -v size >/dev/null 2>&1; then
    echo "error: platform 'size' tool is required for the size gate" >&2
    exit 2
fi

mkdir -p "$MEASURE_DIR"

# Measures one static library's post-strip linkable sections. Task 22 (D9)
# added the FFI staticlib as a second measured artifact under the same bar:
# adding an artifact strengthens the gate, and the core measurement is never
# traded away for it. Moving the 5,120 KB bar itself remains an owner call.
measure_artifact() {
    local label="$1"
    local artifact="$2"
    local gate="${3:-gate}"
    local stripped="$MEASURE_DIR/$(basename "${artifact%.a}")-stripped.a"
    local size_bytes size_kb archive_kb

    if [[ ! -f "$artifact" ]]; then
        echo "error: expected static library not found at $artifact" >&2
        exit 2
    fi

    cp "$artifact" "$stripped"
    case "$(uname -s)" in
        Darwin)
            strip -S -x "$stripped"
            size_bytes="$(size -m "$stripped" | awk '
                /^[[:space:]]*Section \(/ && $0 !~ /\(__LLVM,/ { total += $NF }
                END { print total + 0 }
            ')"
            ;;
        Linux)
            strip --strip-unneeded "$stripped"
            size_bytes="$(size -A "$stripped" | awk '
                $1 ~ /^\./ && $1 !~ /^\.llvm/ { total += $2 }
                END { print total + 0 }
            ')"
            ;;
        *)
            echo "error: size-budget.sh supports Darwin and Linux" >&2
            exit 2
            ;;
    esac

    size_kb="$(( (size_bytes + 1023) / 1024 ))"
    archive_kb="$(du -k "$stripped" | awk '{print $1}')"
    if [[ "$gate" == "gate" ]]; then
        echo "$label stripped staticlib linked size: $size_kb KB (archive: $archive_kb KB; budget: $BUDGET_KB KB)"
    else
        echo "$label stripped staticlib linked size: $size_kb KB (archive: $archive_kb KB; recorded only; no budget introduced)"
    fi

    if [[ "$gate" == "gate" ]] && (( size_kb > BUDGET_KB )); then
        echo "error: $label stripped staticlib linked size $size_kb KB exceeds budget $BUDGET_KB KB" >&2
        exit 1
    fi
}

measure_artifact "core" "$TARGET_ROOT/release/libzeppelin_embed.a"
measure_artifact "ffi" "$TARGET_ROOT/release/libzeppelin_embed_ffi.a"
if [[ "$(uname -s)" == "Darwin" ]]; then
    measure_artifact "text" "$TARGET_ROOT/release/libzeppelin_embed_text.a" "report"
fi

CONSUMER_MANIFEST="$PROJECT_ROOT/tools/size-consumer/Cargo.toml"
CONSUMER_TARGET="$MEASURE_DIR/consumer-target"
cargo build --offline --locked --release --manifest-path "$CONSUMER_MANIFEST" --target-dir "$CONSUMER_TARGET"
CONSUMER_BINARY="$CONSUMER_TARGET/release/zeppelin-embed-size-consumer"
CONSUMER_STRIPPED="$MEASURE_DIR/zeppelin-embed-size-consumer-stripped"
if [[ ! -f "$CONSUMER_BINARY" ]]; then
    echo "error: expected minimal consumer binary not found at $CONSUMER_BINARY" >&2
    exit 2
fi
cp "$CONSUMER_BINARY" "$CONSUMER_STRIPPED"
case "$(uname -s)" in
    Darwin)
        strip -S -x "$CONSUMER_STRIPPED"
        CONSUMER_SIZE_BYTES="$(size -m "$CONSUMER_STRIPPED" | awk '
            /^[[:space:]]*Section / && $0 !~ /\(__LLVM,/ {
                bytes = ($NF == "(zerofill)") ? $(NF - 1) : $NF
                total += bytes
            }
            END { print total + 0 }
        ')"
        ;;
    Linux)
        strip --strip-unneeded "$CONSUMER_STRIPPED"
        CONSUMER_SIZE_BYTES="$(size -A "$CONSUMER_STRIPPED" | awk '
            $1 ~ /^\./ && $1 !~ /^\.llvm/ { total += $2 }
            END { print total + 0 }
        ')"
        ;;
esac
CONSUMER_SIZE_KB="$(( (CONSUMER_SIZE_BYTES + 1023) / 1024 ))"
CONSUMER_FILE_KB="$(du -k "$CONSUMER_STRIPPED" | awk '{print $1}')"
echo "minimal zeppelin-embed consumer stripped linked size: $CONSUMER_SIZE_KB KB (file: $CONSUMER_FILE_KB KB; reported only; no budget introduced)"
