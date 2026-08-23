#!/usr/bin/env python3
"""Generate the tokenizer's NFKC search-folding table.

This is a developer tool run by hand, NOT a build script: build scripts and
host probing are prohibited (root CLAUDE.md). Its output is committed as
`crates/zeppelin-embed/src/fts/tokenizer/fold_table.rs` and reviewed like any
other source file. Re-run it only to adopt a new Unicode version, which is a
deliberate tokenizer-epoch bump (task 21 owns migration).

Usage:
    python3 tools/generate-fold-table.py \
        > crates/zeppelin-embed/src/fts/tokenizer/fold_table.rs

The fold is Lucene's ICUFoldingFilter shape: NFKC, then Unicode case folding,
then canonical decomposition with nonspacing marks removed, then recomposed.
That single per-codepoint mapping collapses case, compatibility forms
(ligatures, fullwidth, super/subscripts), and diacritics into one lookup, so
`Café`, `CAFÉ`, `café`, and `ｃａｆｅ` all reach the term `cafe`.

Only codepoints whose fold differs from themselves are emitted. Everything
else, ASCII included, is handled by the caller's fast path.
"""

import sys
import unicodedata as ud

SURROGATES = range(0xD800, 0xE000)


def is_kana(char: str) -> bool:
    """True for Hiragana and Katakana, including phonetic extensions."""
    code = ord(char)
    return (
        0x3040 <= code <= 0x309F  # Hiragana
        or 0x30A0 <= code <= 0x30FF  # Katakana
        or 0x31F0 <= code <= 0x31FF  # Katakana Phonetic Extensions
        or 0xFF66 <= code <= 0xFF9F  # Halfwidth Katakana
    )


def search_fold(codepoint: int) -> str:
    """Returns the folded form of one codepoint.

    Diacritic stripping is skipped for kana. In Latin scripts a combining
    mark is a spelling detail and `café` should find `cafe`, but in Japanese
    the dakuten is phonemic: stripping it turns が (ga) into か (ka), a
    different mora and a different word. Lucene's ICUFoldingFilter carries
    the same hazard, which is why its Japanese analyzer does not use it.
    """
    text = chr(codepoint)
    text = ud.normalize("NFKC", text)
    text = text.casefold()
    text = ud.normalize("NFKC", text)
    if text and is_kana(text[0]):
        return text
    text = ud.normalize("NFD", text)
    text = "".join(c for c in text if ud.category(c) != "Mn")
    return ud.normalize("NFC", text)


def escape(text: str) -> str:
    out = []
    for char in text:
        if char == "\\":
            out.append("\\\\")
        elif char == '"':
            out.append('\\"')
        elif 0x20 <= ord(char) < 0x7F:
            out.append(char)
        else:
            out.append(f"\\u{{{ord(char):X}}}")
    return "".join(out)


def main() -> None:
    keys: list[int] = []
    folds: list[str] = []
    for codepoint in range(0x110000):
        if codepoint in SURROGATES:
            continue
        folded = search_fold(codepoint)
        if folded != chr(codepoint):
            keys.append(codepoint)
            folds.append(folded)

    blob = "".join(folds)
    offsets: list[int] = []
    cursor = 0
    for folded in folds:
        offsets.append(cursor)
        cursor += len(folded.encode("utf-8"))
    offsets.append(cursor)

    write = sys.stdout.write
    write("//! Generated NFKC search-folding table. DO NOT EDIT BY HAND.\n")
    write("//!\n")
    write("//! Regenerate with `python3 tools/generate-fold-table.py`. Changing this\n")
    write("//! file changes the meaning of every index built under the current\n")
    write("//! tokenizer epoch, so a regeneration is an epoch bump, never a fix.\n")
    write("\n")
    write(f'/// Unicode Character Database version this table was generated from.\n')
    write(f'pub(crate) const UNICODE_VERSION: &str = "{ud.unidata_version}";\n\n')
    write("/// Codepoints whose search fold differs from themselves, ascending.\n")
    write(f"pub(crate) static FOLD_KEYS: [u32; {len(keys)}] = [\n")
    for index in range(0, len(keys), 12):
        row = ", ".join(f"0x{k:04X}" for k in keys[index : index + 12])
        write(f"    {row},\n")
    write("];\n\n")
    write("/// Byte offsets into `FOLD_BLOB`; entry `i` spans `[i]..[i + 1]`.\n")
    write(f"pub(crate) static FOLD_OFFSETS: [u32; {len(offsets)}] = [\n")
    for index in range(0, len(offsets), 12):
        row = ", ".join(str(o) for o in offsets[index : index + 12])
        write(f"    {row},\n")
    write("];\n\n")
    write("/// Concatenated folded forms addressed by `FOLD_OFFSETS`.\n")
    write(f'pub(crate) static FOLD_BLOB: &str = "{escape(blob)}";\n')


if __name__ == "__main__":
    main()
