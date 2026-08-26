#!/bin/bash

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
BUILD_DIR="$ROOT_DIR/target/xcframework"
WORK_DIR="$BUILD_DIR/work"
HEADERS_DIR="$WORK_DIR/headers"
ARTIFACT="$BUILD_DIR/ZeppelinEmbed.xcframework"
ARCHIVE_ZIP="$BUILD_DIR/ZeppelinEmbed.xcframework.zip"
SLICE_EVIDENCE="$ROOT_DIR/tasks/evidence/23-slices.md"
SIZE_EVIDENCE="$ROOT_DIR/tasks/evidence/23-size.md"
ALLOWLIST="$ROOT_DIR/crates/zeppelin-embed-ffi/symbols.allowlist"
PRIVACY_MANIFEST="$SCRIPT_DIR/PrivacyInfo.xcprivacy"
RUST_LLVM_NM="$(rustc --print sysroot)/lib/rustlib/aarch64-apple-darwin/bin/llvm-nm"
SIZE_BUDGET_KB=5120

mkdir -p "$BUILD_DIR" "$ROOT_DIR/tasks/evidence"
rm -rf "$WORK_DIR" "$ARTIFACT"
rm -f "$ARCHIVE_ZIP"
mkdir -p "$HEADERS_DIR"
cp "$ROOT_DIR/crates/zeppelin-embed-ffi/include/zeppelin_embed.h" "$HEADERS_DIR/"
cp "$SCRIPT_DIR/module.modulemap" "$HEADERS_DIR/"

{
    echo "# Task 23 XCFramework slice evidence"
    echo
    echo "Hardware: $(uname -a)"
    echo "Rust: $(rustc --version)"
    echo "Swift: $(swift --version | head -n 1)"
    echo
} > "$SLICE_EVIDENCE"

{
    echo "# Task 23 XCFramework size evidence"
    echo
    echo "Hardware: $(uname -a)"
    echo "Budget: $SIZE_BUDGET_KB KB linked sections per architecture slice"
    echo "Measurement: copy archive, strip -S -x, then size -m; __LLVM is excluded"
    echo
} > "$SIZE_EVIDENCE"

built_labels=()
built_archives=()
linked_section_total_kb=0

compatible_nm() {
    local archive="$1"
    local output="$2"
    if nm -gU "$archive" > "$output" 2> "$output.stderr"; then
        return 0
    fi
    if grep -q "Unknown attribute kind" "$output.stderr" && [ -x "$RUST_LLVM_NM" ]; then
        "$RUST_LLVM_NM" --extern-only --defined-only "$archive" > "$output" 2> "$output.stderr"
        return $?
    fi
    cat "$output.stderr" >&2
    return 1
}

check_export_allowlist() {
    local label="$1"
    local archive="$2"
    local symbols="$WORK_DIR/$label.symbols"
    local observed="$WORK_DIR/$label.ze-symbols"
    compatible_nm "$archive" "$symbols" || return 1
    awk '{ symbol=$NF; sub(/^_/, "", symbol); if (symbol ~ /^ze_/) print symbol }' \
        "$symbols" | LC_ALL=C sort -u > "$observed"
    if ! diff -u "$ALLOWLIST" "$observed"; then
        echo "ERROR: $label exported C namespace differs from symbols.allowlist" >&2
        return 1
    fi
    echo "nm allowlist: PASS ($label)"
}

check_privacy_symbols() {
    local label="$1"
    local archive="$2"
    local undefined="$WORK_DIR/$label.undefined-symbols"
    "$RUST_LLVM_NM" --undefined-only "$archive" > "$undefined" 2> "$undefined.stderr"
    local mapping=(
        "stat:NSPrivacyAccessedAPICategoryFileTimestamp:C617.1"
        "fstat:NSPrivacyAccessedAPICategoryFileTimestamp:C617.1"
        "lstat:NSPrivacyAccessedAPICategoryFileTimestamp:C617.1"
        "statfs:NSPrivacyAccessedAPICategoryDiskSpace:85F4.1"
        "fstatfs:NSPrivacyAccessedAPICategoryDiskSpace:85F4.1"
        "mach_absolute_time:NSPrivacyAccessedAPICategorySystemBootTime:35F9.1"
    )
    local entry symbol category reason
    for entry in "${mapping[@]}"; do
        IFS=: read -r symbol category reason <<< "$entry"
        if grep -Eq "(^|_)${symbol}$" "$undefined"; then
            grep -q "$category" "$PRIVACY_MANIFEST" || return 1
            grep -q "$reason" "$PRIVACY_MANIFEST" || return 1
        fi
    done
    echo "privacy required-reason symbol scan: PASS ($label)"
}

measure_slice() {
    local label="$1"
    local archive="$2"
    local stripped="$WORK_DIR/$label-stripped.a"
    local size_output="$WORK_DIR/$label-size.txt"
    local linked_bytes linked_kb text_bytes text_kb archive_kb
    cp "$archive" "$stripped"
    strip -S -x "$stripped"
    size -m "$stripped" > "$size_output"
    linked_bytes="$(awk '
        /^[[:space:]]*Section \(/ && $0 !~ /\(__LLVM,/ { total += $NF }
        END { print total + 0 }
    ' "$size_output")"
    text_bytes="$(awk '
        /^[[:space:]]*Section \(__TEXT,/ { total += $NF }
        END { print total + 0 }
    ' "$size_output")"
    linked_kb="$(( (linked_bytes + 1023) / 1024 ))"
    text_kb="$(( (text_bytes + 1023) / 1024 ))"
    archive_kb="$(du -k "$stripped" | awk '{print $1}')"
    linked_section_total_kb="$((linked_section_total_kb + linked_kb))"
    {
        echo "## $label"
        echo
        echo '```text'
        echo "strip -S -x $stripped"
        echo "size -m $stripped"
        echo "linked_section_bytes=$linked_bytes"
        echo "linked_section_kb=$linked_kb"
        echo "__TEXT_bytes=$text_bytes"
        echo "__TEXT_kb=$text_kb"
        echo "stripped_archive_kb=$archive_kb"
        echo "budget_kb=$SIZE_BUDGET_KB"
        echo '```'
        echo
    } >> "$SIZE_EVIDENCE"
    if [ "$linked_kb" -gt "$SIZE_BUDGET_KB" ]; then
        echo "ERROR: $label linked sections $linked_kb KB exceed $SIZE_BUDGET_KB KB" >&2
        return 1
    fi
    echo "size gate: PASS ($label $linked_kb KB linked, __TEXT $text_kb KB)"
}

attempt_slice() {
    local label="$1"
    local target="$2"
    local sdk="$3"
    local sdk_path log archive status
    log="$WORK_DIR/$label.build.log"
    archive="$ROOT_DIR/target/$target/release/libzeppelin_embed_ffi.a"
    sdk_path="$(xcrun --sdk "$sdk" --show-sdk-path 2>&1)"
    {
        echo "## $label"
        echo
        echo '```text'
        echo "SDKROOT=$sdk_path cargo build -p zeppelin-embed-ffi --release --target $target"
    } >> "$SLICE_EVIDENCE"
    if SDKROOT="$sdk_path" cargo build -p zeppelin-embed-ffi --release --target "$target" \
        > "$log" 2>&1; then
        status=0
    else
        status=$?
    fi
    cat "$log" >> "$SLICE_EVIDENCE"
    {
        echo "exit_code=$status"
        echo '```'
        echo
    } >> "$SLICE_EVIDENCE"
    if [ "$status" -ne 0 ] || [ ! -f "$archive" ]; then
        echo "slice $label: NOT BUILT (exit $status)"
        return 0
    fi
    check_export_allowlist "$label" "$archive" || return 1
    check_privacy_symbols "$label" "$archive" || return 1
    measure_slice "$label" "$archive" || return 1
    built_labels+=("$label")
    built_archives+=("$archive")
    echo "slice $label: BUILT"
}

cd "$ROOT_DIR"
attempt_slice "macos-arm64" "aarch64-apple-darwin" "macosx" || exit 1
attempt_slice "macos-x86_64" "x86_64-apple-darwin" "macosx" || exit 1
attempt_slice "ios-arm64" "aarch64-apple-ios" "iphoneos" || exit 1
attempt_slice "ios-simulator-arm64" "aarch64-apple-ios-sim" "iphonesimulator" || exit 1
attempt_slice "ios-simulator-x86_64" "x86_64-apple-ios" "iphonesimulator" || exit 1

if [ "${#built_archives[@]}" -eq 0 ]; then
    echo "ERROR: no XCFramework slice could be built" >&2
    exit 1
fi
if [[ ! " ${built_labels[*]} " =~ " macos-arm64 " ]]; then
    echo "ERROR: required macos-arm64 slice did not build" >&2
    exit 1
fi

macos_inputs=()
simulator_inputs=()
ios_device=""
for index in "${!built_labels[@]}"; do
    case "${built_labels[$index]}" in
        macos-*) macos_inputs+=("${built_archives[$index]}") ;;
        ios-arm64) ios_device="${built_archives[$index]}" ;;
        ios-simulator-*) simulator_inputs+=("${built_archives[$index]}") ;;
    esac
done

xcframework_args=()
combine_platform() {
    local output="$1"
    shift
    local inputs=("$@")
    if [ "${#inputs[@]}" -eq 1 ]; then
        cp "${inputs[0]}" "$output"
    else
        lipo -create "${inputs[@]}" -output "$output"
    fi
    xcframework_args+=("-library" "$output" "-headers" "$HEADERS_DIR")
}

if [ "${#macos_inputs[@]}" -gt 0 ]; then
    combine_platform "$WORK_DIR/libzeppelin_embed_ffi-macos.a" "${macos_inputs[@]}"
fi
if [ -n "$ios_device" ]; then
    cp "$ios_device" "$WORK_DIR/libzeppelin_embed_ffi-ios.a"
    xcframework_args+=(
        "-library" "$WORK_DIR/libzeppelin_embed_ffi-ios.a"
        "-headers" "$HEADERS_DIR"
    )
fi
if [ "${#simulator_inputs[@]}" -gt 0 ]; then
    combine_platform "$WORK_DIR/libzeppelin_embed_ffi-ios-simulator.a" \
        "${simulator_inputs[@]}"
fi

xcodebuild -create-xcframework "${xcframework_args[@]}" -output "$ARTIFACT"

# xcodebuild emits AvailableLibraries in an arbitrary order, so two builds of
# identical inputs differ only by a permutation of that array. The entries have
# the same total byte length either way, which is why the artifact size is
# stable while its checksum is not. Sort by LibraryIdentifier so the pinned
# checksum is reproducible.
python3 - "$ARTIFACT/Info.plist" <<'NORMALISE_PLIST'
import plistlib
import sys

path = sys.argv[1]
with open(path, "rb") as handle:
    plist = plistlib.load(handle)
plist["AvailableLibraries"] = sorted(
    plist.get("AvailableLibraries", []),
    key=lambda entry: entry.get("LibraryIdentifier", ""),
)
with open(path, "wb") as handle:
    plistlib.dump(plist, handle, sort_keys=True)
NORMALISE_PLIST

cp "$PRIVACY_MANIFEST" "$ARTIFACT/PrivacyInfo.xcprivacy"
while IFS= read -r slice_dir; do
    cp "$PRIVACY_MANIFEST" "$slice_dir/PrivacyInfo.xcprivacy"
done < <(find "$ARTIFACT" -mindepth 1 -maxdepth 1 -type d | LC_ALL=C sort)

find "$ARTIFACT" -exec touch -t 202001010000 {} +
(
    cd "$BUILD_DIR"
    find "$(basename "$ARTIFACT")" -print | LC_ALL=C sort | \
        zip -X -q "$ARCHIVE_ZIP" -@
)
checksum="$(shasum -a 256 "$ARCHIVE_ZIP" | awk '{print $1}')"
echo "$checksum" > "$BUILD_DIR/checksum.txt"
xcframework_kb="$(du -sk "$ARTIFACT" | awk '{print $1}')"
zip_kb="$(du -k "$ARCHIVE_ZIP" | awk '{print $1}')"
{
    echo "## Totals (reported, not gated)"
    echo
    echo '```text'
    echo "built_architecture_slices=${#built_archives[@]}"
    echo "linked_section_total_kb=$linked_section_total_kb"
    echo "xcframework_total_kb=$xcframework_kb"
    echo "xcframework_zip_kb=$zip_kb"
    echo '```'
} >> "$SIZE_EVIDENCE"

pin="$ROOT_DIR/swift/ZeppelinEmbed/binary-checksum.txt"
if [ -f "$pin" ] && [ "$(tr -d '[:space:]' < "$pin")" != "$checksum" ]; then
    echo "ERROR: binary-checksum.txt does not match $checksum" >&2
    exit 1
fi

test -f "$ARTIFACT/PrivacyInfo.xcprivacy"
for required in C617.1 85F4.1 35F9.1; do
    grep -q "$required" "$ARTIFACT/PrivacyInfo.xcprivacy"
done

echo "XCFramework: $ARTIFACT"
echo "Slices built: ${built_labels[*]}"
echo "Checksum: $checksum"
