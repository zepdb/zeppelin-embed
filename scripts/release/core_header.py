#!/usr/bin/env python3
"""Write the public header for a core-only Zeppelin Embed binary."""

from __future__ import annotations

import argparse
from pathlib import Path

TEXT_FUNCTIONS = {
    "ze_text_ingest",
    "ze_text_maintain",
    "ze_text_open",
    "ze_text_query",
    "ze_text_query_result_free",
}
TEXT_TYPES = {
    "ZeTextDocument",
    "ZeTextIngestRequest",
    "ZeTextOpenRequest",
    "ZeTextQueryHit",
    "ZeTextQueryRequest",
    "ZeTextQueryResult",
}


def remove_trailing_documentation(output: list[str], item: str) -> None:
    while output and not output[-1].strip():
        output.pop()
    if not output or output[-1].strip() != "*/":
        raise RuntimeError(f"optional text ABI item has no documentation: {item}")
    while output and output[-1].strip() != "/*":
        output.pop()
    if not output:
        raise RuntimeError(f"unterminated documentation for: {item}")
    output.pop()
    output.append("\n")


def write_core_header(source: Path, destination: Path) -> None:
    lines = source.read_text().splitlines(keepends=True)
    output: list[str] = []
    removed_functions: set[str] = set()
    removed_types: set[str] = set()
    skipping_function = False
    skipping_type: str | None = None

    for line in lines:
        if skipping_type is not None:
            if line.strip() == f"}} {skipping_type};":
                skipping_type = None
            continue
        if skipping_function:
            if line.rstrip().endswith(";"):
                skipping_function = False
            continue
        if line.startswith("typedef struct ZeText"):
            item = line.split()[2]
            if item not in TEXT_TYPES:
                raise RuntimeError(f"unexpected text ABI type: {item}")
            remove_trailing_documentation(output, item)
            removed_types.add(item)
            skipping_type = item
            continue
        if line.startswith("ze_error_code ze_text_"):
            function = line.split("(", 1)[0].split()[-1]
            if function not in TEXT_FUNCTIONS:
                raise RuntimeError(f"unexpected text ABI function: {function}")
            remove_trailing_documentation(output, function)
            removed_functions.add(function)
            skipping_function = not line.rstrip().endswith(";")
            continue
        output.append(line)

    if skipping_function:
        raise RuntimeError("unterminated text ABI declaration")
    if skipping_type is not None:
        raise RuntimeError(f"unterminated text ABI type: {skipping_type}")
    if removed_functions != TEXT_FUNCTIONS:
        missing = ", ".join(sorted(TEXT_FUNCTIONS - removed_functions))
        raise RuntimeError(
            f"generated header is missing text ABI declarations: {missing}"
        )
    if removed_types != TEXT_TYPES:
        missing = ", ".join(sorted(TEXT_TYPES - removed_types))
        raise RuntimeError(f"generated header is missing text ABI types: {missing}")

    contents = "".join(output)
    while "\n\n\n" in contents:
        contents = contents.replace("\n\n\n", "\n\n")
    destination.write_text(contents)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("source", type=Path)
    parser.add_argument("destination", type=Path)
    arguments = parser.parse_args()
    write_core_header(arguments.source, arguments.destination)


if __name__ == "__main__":
    main()
