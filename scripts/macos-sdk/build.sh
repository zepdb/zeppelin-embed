#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
BUILD_DIR="$ROOT_DIR/target/macos-sdk"
STAGE_DIR="$BUILD_DIR/zeppelin-embed-macos-arm64"
ARCHIVE="$BUILD_DIR/zeppelin-embed-macos-arm64.tar.gz"
TARGET_TRIPLE="aarch64-apple-darwin"
MACOS_DEPLOYMENT_TARGET="11.0"
RUST_OUTPUT="$ROOT_DIR/target/$TARGET_TRIPLE/release"
SYMBOL_ALLOWLIST="$ROOT_DIR/crates/zeppelin-embed-ffi/symbols.allowlist"

mkdir -p "$BUILD_DIR"
rm -rf "$STAGE_DIR"
rm -f "$ARCHIVE"
mkdir -p "$STAGE_DIR/include" "$STAGE_DIR/lib"

cd "$ROOT_DIR"
MACOSX_DEPLOYMENT_TARGET="$MACOS_DEPLOYMENT_TARGET" \
    cargo build --locked --release -p zeppelin-embed-ffi --target "$TARGET_TRIPLE"

static_library="$RUST_OUTPUT/libzeppelin_embed_ffi.a"
dynamic_library="$RUST_OUTPUT/libzeppelin_embed_ffi.dylib"
test -f "$static_library"
test -f "$dynamic_library"
lipo "$static_library" -verify_arch arm64
lipo "$dynamic_library" -verify_arch arm64

python3 "$ROOT_DIR/scripts/release/core_header.py" \
    "$ROOT_DIR/crates/zeppelin-embed-ffi/include/zeppelin_embed.h" \
    "$STAGE_DIR/include/zeppelin_embed.h"
cp "$static_library" "$STAGE_DIR/lib/"
cp "$dynamic_library" "$STAGE_DIR/lib/"
install_name_tool -id '@rpath/libzeppelin_embed_ffi.dylib' \
    "$STAGE_DIR/lib/libzeppelin_embed_ffi.dylib"
codesign --force --sign - "$STAGE_DIR/lib/libzeppelin_embed_ffi.dylib"
cp "$ROOT_DIR/LICENSE" "$STAGE_DIR/"
cp "$SCRIPT_DIR/README.md" "$STAGE_DIR/"

expected_symbols="$BUILD_DIR/expected-symbols"
observed_symbols="$BUILD_DIR/observed-symbols"
grep -v '^ze_text_' "$SYMBOL_ALLOWLIST" > "$expected_symbols"
nm -gU "$dynamic_library" | awk '
    { symbol=$NF; sub(/^_/, "", symbol); if (symbol ~ /^ze_/) print symbol }
' | LC_ALL=C sort -u > "$observed_symbols"
diff -u "$expected_symbols" "$observed_symbols"

if grep -En 'ZeText|ze_text_' "$STAGE_DIR/include/zeppelin_embed.h"; then
    echo "ERROR: core SDK header exposes the optional text ABI" >&2
    exit 1
fi

minimum_macos="$(vtool -show-build "$STAGE_DIR/lib/libzeppelin_embed_ffi.dylib" | \
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
