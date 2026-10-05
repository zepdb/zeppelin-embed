#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
ARTIFACT=legacy
PREBUILT_DIR=""
while [ $# -gt 0 ]; do
    case "$1" in
        --artifact) ARTIFACT="$2"; shift 2 ;;
        --prebuilt-dir) PREBUILT_DIR="$2"; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
features=()
case "$ARTIFACT" in
    legacy) SUFFIX=""; MACOS_DEPLOYMENT_TARGET=11.0 ;;
    graph-cypher) SUFFIX="-graph-cypher"; MACOS_DEPLOYMENT_TARGET=14.0; features=(--features graph-cypher) ;;
    *) echo "unknown artifact: $ARTIFACT" >&2; exit 2 ;;
esac
BUILD_DIR="$ROOT_DIR/target/macos-sdk$SUFFIX"
STAGE_DIR="$BUILD_DIR/zeppelin-embed$SUFFIX-macos-arm64"
ARCHIVE="$BUILD_DIR/zeppelin-embed$SUFFIX-macos-arm64.tar.gz"
TARGET_TRIPLE="aarch64-apple-darwin"
RUST_OUTPUT="$BUILD_DIR/cargo/$TARGET_TRIPLE/release"
LIBRARY_NAME="libzeppelin_embed${SUFFIX//-/_}_ffi"

mkdir -p "$BUILD_DIR"
rm -rf "$STAGE_DIR"
rm -f "$ARCHIVE"
mkdir -p "$STAGE_DIR/include" "$STAGE_DIR/lib"

cd "$ROOT_DIR"
if [ -n "$PREBUILT_DIR" ]; then
    RUST_OUTPUT="$PREBUILT_DIR"
else
    CARGO_BUILD_JOBS=3 MACOSX_DEPLOYMENT_TARGET="$MACOS_DEPLOYMENT_TARGET" \
        cargo build --locked --release --no-default-features -p zeppelin-embed-ffi \
        --target "$TARGET_TRIPLE" --target-dir "$BUILD_DIR/cargo" "${features[@]}"
fi

static_library="$RUST_OUTPUT/libzeppelin_embed_ffi.a"
dynamic_library="$RUST_OUTPUT/libzeppelin_embed_ffi.dylib"
test -f "$static_library"
test -f "$dynamic_library"
lipo "$static_library" -verify_arch arm64
lipo "$dynamic_library" -verify_arch arm64

python3 "$ROOT_DIR/scripts/release/core_header.py" \
    "$ROOT_DIR/crates/zeppelin-embed-ffi/include/zeppelin_embed.h" \
    "$STAGE_DIR/include" --artifact "$ARTIFACT"
python3 "$ROOT_DIR/scripts/release/package-native.py" archive \
    "$static_library" "$STAGE_DIR/lib/$LIBRARY_NAME.a"
cp "$dynamic_library" "$STAGE_DIR/lib/$LIBRARY_NAME.dylib"
install_name_tool -id "@rpath/$LIBRARY_NAME.dylib" \
    "$STAGE_DIR/lib/$LIBRARY_NAME.dylib"
codesign --force --sign - "$STAGE_DIR/lib/$LIBRARY_NAME.dylib"
cp "$ROOT_DIR/LICENSE" "$STAGE_DIR/"
cp "$SCRIPT_DIR/README.md" "$STAGE_DIR/"

python3 - "$ROOT_DIR" "$static_library" "$dynamic_library" "$STAGE_DIR/lib/$LIBRARY_NAME.a" "$STAGE_DIR/lib/$LIBRARY_NAME.dylib" "$ARTIFACT" <<'CHECK'
import importlib.util
from pathlib import Path
import sys
spec = importlib.util.spec_from_file_location('check', Path(sys.argv[1]) / 'scripts/release/check-graph-artifact.py')
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
for path in sys.argv[2:-1]:
    m.check_exports(Path(path), sys.argv[-1])
CHECK

if grep -En 'ZeText|ze_text_' "$STAGE_DIR/include/zeppelin_embed.h"; then
    echo "ERROR: core SDK header exposes the optional text ABI" >&2
    exit 1
fi

minimum_macos="$(vtool -show-build "$STAGE_DIR/lib/$LIBRARY_NAME.dylib" | \
    awk '/minos/ { print $2; exit }')"
if [ "$minimum_macos" != "$MACOS_DEPLOYMENT_TARGET" ]; then
    echo "ERROR: dylib targets macOS $minimum_macos, expected $MACOS_DEPLOYMENT_TARGET" >&2
    exit 1
fi

find "$STAGE_DIR" -exec touch -t 202001010000 {} +
COPYFILE_DISABLE=1 tar -C "$BUILD_DIR" -czf "$ARCHIVE" "$(basename "$STAGE_DIR")"

echo "macOS SDK: $ARCHIVE"
echo "Architecture: arm64"
echo "Minimum macOS: $minimum_macos"
echo "SHA-256: $(shasum -a 256 "$ARCHIVE" | awk '{print $1}')"
