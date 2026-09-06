"""Store typed dictation notes, page them, and run a filtered search."""

from __future__ import annotations

import tempfile
from pathlib import Path

import numpy as np

import zeppelin_embed as ze


def main() -> int:
    schema = ze.NamespaceSpec(
        attributes=(
            ze.AttributeDefinition(1, "priority", ze.AttributeType.U64),
            ze.AttributeDefinition(2, "reviewed", ze.AttributeType.BOOL),
            ze.AttributeDefinition(3, "category", ze.AttributeType.DICTIONARY_STRING),
            ze.AttributeDefinition(4, "project", ze.AttributeType.RAW_STRING, nullable=True),
        ),
        vector_space=ze.VectorSpace(2, normalization=ze.Normalization.UNIT_L2),
    )

    # Each namespace lives at root/name on disk. The temporary root keeps the
    # example self-cleaning; use an application path to reopen the data later.
    with tempfile.TemporaryDirectory() as scratch:
        root = Path(scratch)
        with ze.open_namespace(root, "notes", schema) as notes:
            documents = (
                ze.StoredDocument(
                    101,
                    timestamp=100,
                    vector=np.asarray([1.0, 0.0], dtype=np.float32),
                    text="Plan the product launch",
                    attributes=(
                        ze.AttributeValue(1, ze.AttributeType.U64, 2),
                        ze.AttributeValue(2, ze.AttributeType.BOOL, True),
                        ze.AttributeValue(3, ze.AttributeType.DICTIONARY_STRING, "work"),
                        ze.AttributeValue(4, ze.AttributeType.RAW_STRING, "zeppelin"),
                    ),
                ),
                ze.StoredDocument(
                    102,
                    timestamp=300,
                    vector=np.asarray([0.0, 1.0], dtype=np.float32),
                    text="Buy oat milk",
                    attributes=(
                        ze.AttributeValue(1, ze.AttributeType.U64, 1),
                        ze.AttributeValue(2, ze.AttributeType.BOOL, False),
                        ze.AttributeValue(3, ze.AttributeType.DICTIONARY_STRING, "personal"),
                        ze.AttributeValue(4, ze.AttributeType.RAW_STRING, None),
                    ),
                ),
                ze.StoredDocument(
                    103,
                    timestamp=200,
                    vector=np.asarray([0.8, 0.6], dtype=np.float32),
                    text="Review search benchmarks",
                    attributes=(
                        ze.AttributeValue(1, ze.AttributeType.U64, 3),
                        ze.AttributeValue(2, ze.AttributeType.BOOL, True),
                        ze.AttributeValue(3, ze.AttributeType.DICTIONARY_STRING, "work"),
                        ze.AttributeValue(4, ze.AttributeType.RAW_STRING, "zeppelin"),
                    ),
                ),
                ze.StoredDocument(
                    104,
                    timestamp=400,
                    vector=np.asarray([0.6, 0.8], dtype=np.float32),
                    text="Book dentist appointment",
                    attributes=(
                        ze.AttributeValue(1, ze.AttributeType.U64, 2),
                        ze.AttributeValue(2, ze.AttributeType.BOOL, False),
                        ze.AttributeValue(3, ze.AttributeType.DICTIONARY_STRING, "personal"),
                        ze.AttributeValue(4, ze.AttributeType.RAW_STRING, None),
                    ),
                ),
            )
            mutation = notes.upsert(documents)
            print(f"upserted 4 notes at generation {mutation.generation}")

            # get preserves caller order and returns None for a missing id.
            lookup = notes.get([101, 999])
            found, missing = lookup.documents
            if found is None:
                raise RuntimeError("note 101 unexpectedly missing")
            print(f'get 101: "{found.text}"')
            if missing is None:
                print(f"get 999: missing ({lookup.missing_count} missing)")

            # The binding flattens this structured AND into the C ABI's
            # contiguous child-node range.
            selected = ze.Filter.and_(
                ze.Filter.eq("category", "work"),
                ze.Filter.range_("priority", gte=2, lte=3),
            )
            cursor = None
            scanned = 0
            page_number = 1
            while True:
                page = notes.scan(
                    cursor=cursor,
                    limit=1,
                    order="timestamp_ascending",
                    filter=selected,
                )
                for note in page.documents:
                    print(
                        f'scan page {page_number}: note {note.doc_id} '
                        f'at {note.timestamp}: "{note.text}"'
                    )
                scanned += len(page.documents)
                cursor = page.cursor
                if cursor is None:
                    break
                page_number += 1

            count = notes.count(filter=selected)
            print(f"count: {count.count} matching notes (scan found {scanned})")

            matches = notes.search(
                np.asarray([1.0, 0.0], dtype=np.float32),
                k=2,
                tier=ze.Tier.EXACT,
                filter=selected,
            )
            for rank, hit in enumerate(matches.hits, start=1):
                print(f"search {rank}: note {hit.doc_id}, score {hit.score:.3f}")

        # Omitting a vector space creates a plain record store. Vector search
        # on this namespace is rejected with ZE_ERR_NO_VECTOR_SPACE.
        with ze.open_namespace(root, "inbox", ze.NamespaceSpec()) as inbox:
            inbox.upsert([ze.StoredDocument(201, timestamp=500, text="Call Alice")])
            record = inbox.scan(limit=10, order="timestamp_ascending").documents[0]
            print(f'record-only scan: note {record.doc_id}: "{record.text}"')

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
