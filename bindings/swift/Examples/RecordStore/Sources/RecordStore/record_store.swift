import Foundation
import ZeppelinEmbed

@main
struct RecordStore {
    static func main() async throws {
        let schema = NamespaceSpec(
            attributes: [
                AttributeDefinition(id: 1, name: "priority", type: .u64, nullable: false),
                AttributeDefinition(id: 2, name: "reviewed", type: .bool, nullable: false),
                AttributeDefinition(
                    id: 3,
                    name: "category",
                    type: .dictionaryString,
                    nullable: false
                ),
                AttributeDefinition(id: 4, name: "project", type: .rawString, nullable: true),
            ],
            vectorSpace: VectorSpace(dimensions: 2, normalization: .unitL2)
        )

        // Each namespace lives at root/name on disk. This temporary root keeps
        // the example self-cleaning; use an application path to reopen it later.
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("zeppelin-swift-record-store-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: root) }

        let notes = try await ZeppelinStore.openNamespace(root: root, name: "notes", spec: schema)
        let documents = [
            IngestDocument(
                id: DocumentID(high: 0, low: 101),
                revision: 1,
                timestamp: 100,
                vector: [1, 0],
                text: "Plan the product launch",
                attributes: [
                    1: .u64(2), 2: .bool(true),
                    3: .string("work"), 4: .string("zeppelin"),
                ]
            ),
            IngestDocument(
                id: DocumentID(high: 0, low: 102),
                revision: 1,
                timestamp: 300,
                vector: [0, 1],
                text: "Buy oat milk",
                attributes: [
                    1: .u64(1), 2: .bool(false),
                    3: .string("personal"), 4: .null,
                ]
            ),
            IngestDocument(
                id: DocumentID(high: 0, low: 103),
                revision: 1,
                timestamp: 200,
                vector: [0.8, 0.6],
                text: "Review search benchmarks",
                attributes: [
                    1: .u64(3), 2: .bool(true),
                    3: .string("work"), 4: .string("zeppelin"),
                ]
            ),
            IngestDocument(
                id: DocumentID(high: 0, low: 104),
                revision: 1,
                timestamp: 400,
                vector: [0.6, 0.8],
                text: "Book dentist appointment",
                attributes: [
                    1: .u64(2), 2: .bool(false),
                    3: .string("personal"), 4: .null,
                ]
            ),
        ]
        let mutation = try await notes.upsert(documents)
        print("upserted 4 notes at generation \(mutation.generation)")

        // get preserves caller order and returns nil for a missing id.
        let lookup = try await notes.get([
            DocumentID(high: 0, low: 101),
            DocumentID(high: 0, low: 999),
        ])
        guard let found = lookup.documents[0] else {
            throw ZeppelinError.internalError
        }
        print("get 101: \"\(found.text ?? "")\"")
        if lookup.documents[1] == nil {
            print("get 999: missing (\(lookup.missingCount) missing)")
        }

        // The binding flattens this structured AND into the C ABI's
        // contiguous child-node range.
        let selected = Filter.and([
            .eq(3, .string("work")),
            .range(
                1,
                lower: RangeBound(value: .u64(2), inclusive: true),
                upper: RangeBound(value: .u64(3), inclusive: true)
            ),
        ])
        var cursor: ScanCursor?
        var scanned = 0
        var pageNumber = 1
        repeat {
            let page = try await notes.scan(
                after: cursor,
                limit: 1,
                order: .timestampAscending,
                filter: selected
            )
            for note in page.documents {
                print(
                    "scan page \(pageNumber): note \(note.id.low) "
                        + "at \(note.timestamp): \"\(note.text ?? "")\""
                )
            }
            scanned += page.documents.count
            cursor = page.next
            pageNumber += 1
        } while cursor != nil

        let count = try await notes.count(filter: selected)
        print("count: \(count.count) matching notes (scan found \(scanned))")

        let matches = try await notes.search(
            vector: [1, 0],
            filter: selected,
            options: SearchOptions(k: 2, tier: .exact)
        )
        for (index, hit) in matches.hits.enumerated() {
            print(
                "search \(index + 1): note \(hit.documentID?.low ?? 0), "
                    + "score \(String(format: "%.3f", hit.score))"
            )
        }
        try await notes.close()

        // Omitting a vector space creates a plain record store. Vector search
        // on this namespace is rejected with ZE_ERR_NO_VECTOR_SPACE.
        let inbox = try await ZeppelinStore.openNamespace(
            root: root,
            name: "inbox",
            spec: NamespaceSpec(attributes: [], vectorSpace: nil)
        )
        _ = try await inbox.upsert([
            IngestDocument(
                id: DocumentID(high: 0, low: 201),
                revision: 1,
                timestamp: 500,
                vector: [],
                text: "Call Alice"
            )
        ])
        let records = try await inbox.scan(limit: 10, order: .timestampAscending)
        guard let record = records.documents.first else {
            throw ZeppelinError.internalError
        }
        print("record-only scan: note \(record.id.low): \"\(record.text ?? "")\"")
        try await inbox.close()
    }
}
