#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUDGET_KB="${ZE_SIZE_BUDGET_KB:-2048}"

if [[ ! "$BUDGET_KB" =~ ^[0-9]+$ ]]; then
    echo "error: ZE_SIZE_BUDGET_KB must be a non-negative integer, got '$BUDGET_KB'" >&2
    exit 2
fi

cd "$PROJECT_ROOT"
cargo build --release -p zeppelin-embed

TARGET_ROOT="${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}"
ARTIFACT="$TARGET_ROOT/release/libzeppelin_embed.a"
MEASURE_DIR="$TARGET_ROOT/size-budget"
STRIPPED_ARTIFACT="$MEASURE_DIR/libzeppelin_embed-stripped.a"

if [[ ! -f "$ARTIFACT" ]]; then
    echo "error: expected static library not found at $ARTIFACT" >&2
    exit 2
fi
if ! command -v strip >/dev/null 2>&1; then
    echo "error: platform 'strip' tool is required for the size gate" >&2
    exit 2
fi
if ! command -v size >/dev/null 2>&1; then
    echo "error: platform 'size' tool is required for the size gate" >&2
    exit 2
fi

mkdir -p "$MEASURE_DIR"
cp "$ARTIFACT" "$STRIPPED_ARTIFACT"
case "$(uname -s)" in
    Darwin)
        strip -S -x "$STRIPPED_ARTIFACT"
        SIZE_BYTES="$(size -m "$STRIPPED_ARTIFACT" | awk '
            /^[[:space:]]*Section \(/ && $0 !~ /\(__LLVM,/ { total += $NF }
            END { print total + 0 }
        ')"
        ;;
    Linux)
        strip --strip-unneeded "$STRIPPED_ARTIFACT"
        SIZE_BYTES="$(size -A "$STRIPPED_ARTIFACT" | awk '
            $1 ~ /^\./ && $1 !~ /^\.llvm/ { total += $2 }
            END { print total + 0 }
        ')"
        ;;
    *)
        echo "error: size-budget.sh supports Darwin and Linux" >&2
        exit 2
        ;;
esac

SIZE_KB="$(( (SIZE_BYTES + 1023) / 1024 ))"
ARCHIVE_KB="$(du -k "$STRIPPED_ARTIFACT" | awk '{print $1}')"
echo "Stripped staticlib linked size: $SIZE_KB KB (archive: $ARCHIVE_KB KB; budget: $BUDGET_KB KB)"

if (( SIZE_KB > BUDGET_KB )); then
    echo "error: stripped staticlib linked size $SIZE_KB KB exceeds budget $BUDGET_KB KB" >&2
    exit 1
fi
